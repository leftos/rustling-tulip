# fake-claude

A deterministic stand-in for the `claude` CLI the daemon spawns, so tests get a long-running PTY child without a real install, real credentials or a logged-in account. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it.

`index.mjs` prints a stable `[fake-claude] ready` banner, then paints a bottom region the way Claude's TUI does, redrawn in place under the lines it prints, so the daemon's screen rules read it.

The region shows the idle prompt box (`❯ ` between two `─` rules over an auto-mode footer), the `· Cooking…` spinner line over the box while a turn runs, the `⎿  Interrupted · …` row after an Esc, the `✻ Churned for …` line over a `1 shell` footer after a turn that leaves a background shell, and an `AskUserQuestion` dialog ending in `Esc to cancel`.

Every paint sets the title to `◐ <name>` while working and `✳ <name>` otherwise (the name is the last `/rename` title, else `Claude Code`), and a resize repaints the region.

An Esc that arrives alone, with nothing after it for 50 ms, is the Esc key: during a turn it paints the interrupt row and runs no hook, as Claude fires none on Esc, and outside a turn it is ignored; an Esc with anything after it is ordinary input.

It echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, reports one environment variable on `/env <NAME>` as `RT_ENV <NAME> unset` (unset or empty), `RT_ENV <NAME> sha256=<hex>` (a name ending in `_KEY` or `_TOKEN`: the SHA-256 of the UTF-8 value, never the value) or `RT_ENV <NAME> <value>`, and exits on `/exit` or SIGTERM/SIGBREAK.

It accepts and ignores the `--add-dir`, `-p`, `--model` and `--permission-mode` flags the daemon may pass, and takes the conversation id its transcript is named after from `--session-id` or `--resume`. It reads the `--settings` hooks document the daemon injects: on `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` it runs the configured hook commands with the event JSON on stdin.

The prompt cue leaves its turn open under the spinner, the ask cue paints the question dialog and then runs `Stop` 5 s later and paints the idle box, and the bg cue ends its turn with a `Stop` that lists a running background shell (the payloads are in `index.mjs`).

The prompt box echoes what is typed or pasted into it as `❯ <text>`, cut to one row, the way Claude's box shows a draft, so the daemon's chat send sees its message in the box before it presses Enter. Paste markers and control bytes are left out, and nothing is echoed while an EOT-terminated paste is buffered.

Any other submitted line that is not blank and does not start with `/` is a message, and it runs a turn the way the CLI does. The turn writes the user record and runs `UserPromptSubmit`, then paints the spinner. It ends with the assistant's answer (`Done: <text>`), a `system` `turn_duration` record, `Stop`, and the idle box under an `answer:` line.

A message containing `rt-perm` asks a permission before it answers. It writes an assistant `tool_use` record for `Bash` `echo rt-fake` and runs `PermissionRequest`, then paints Claude's permission dialog in place of the prompt box: `Do you want to proceed?` over `1. Yes`, `2. Yes, and don't ask again for: …` and `3. No`, with the `Esc to cancel · Tab to amend` footer.

The digit typed at that dialog, with no Enter, answers it. `1` or `2` writes the `tool_result`, runs `PostToolUse` and ends the turn answering `Ran echo rt-fake and it printed rt-fake.`. `3` writes an error `tool_result` and an interrupted-by-user record, then runs `Stop`.

The transcript goes where the daemon's chat mirror reads it: `<CLAUDE_CONFIG_DIR>/projects/<encoded working directory>/<id>.jsonl`, one JSON record per line, chained by `parentUuid`. The `<id>` comes from the `--session-id` or `--resume` argument. The folder name is the working directory with every character outside `[A-Za-z0-9-]` turned into `-`. A run with no id or no `CLAUDE_CONFIG_DIR` writes no transcript.

A crash is appended to `fake-claude-crash.log` in the OS temp dir.

`fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

The Claude plugin hands this folder to the live tier through `fake_cli` (`crates/agents/claude/src/lib.rs`, feature `test-support`): the harness reads it from every plugin in `agent-registry` and sets the plugin's program variable, `RUSTLING_TULIP_CLAUDE` here, to the shim's path, and the daemon resolves the CLI from that variable at spawn time. Node must be on `PATH`.
