# fake-claude

A deterministic stand-in for the `claude` CLI the daemon spawns, so tests get a long-running PTY child without a real install, real credentials or a logged-in account. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it.

`index.mjs` prints a stable `[fake-claude] ready` banner, then paints a bottom region the way Claude's TUI does, redrawn in place under the lines it prints, so the daemon's screen rules read it.

The region shows the idle prompt box (`❯ ` between two `─` rules over an auto-mode footer), the `· Cooking…` spinner line over the box while a turn runs, the `⎿  Interrupted · …` row after an Esc, the `✻ Churned for …` line over a `1 shell` footer after a turn that leaves a background shell, and an `AskUserQuestion` dialog ending in `Esc to cancel`.

Every paint sets the title to `◐ <name>` while working and `✳ <name>` otherwise (the name is the last `/rename` title, else `Claude Code`), and a resize repaints the region.

An Esc that arrives alone, with nothing after it for 50 ms, is the Esc key: during a turn it paints the interrupt row and runs no hook, as Claude fires none on Esc, and outside a turn it is ignored; an Esc with anything after it is ordinary input.

It echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, reports one environment variable on `/env <NAME>` as `RT_ENV <NAME> unset` (unset or empty), `RT_ENV <NAME> sha256=<hex>` (a name ending in `_KEY` or `_TOKEN`: the SHA-256 of the UTF-8 value, never the value) or `RT_ENV <NAME> <value>`, and exits on `/exit` or SIGTERM/SIGBREAK.

It reads the flags the daemon passes: `-p`/`--print` is print mode, so it prints `[fake-claude] print: <prompt>` and exits 0; otherwise `--` ends the options and a bare argument is the prompt, printed as `[fake-claude] prompt: <prompt>`, and the session stays interactive; the values of `--add-dir`, `--append-system-prompt`, `--effort`, `--model`, `--permission-mode`, `--output-format` and `--verbose` are read and ignored.

It takes the conversation id its transcript is named after from `--session-id` or `--resume`; started with neither (typed into a shell by hand), it mints a uuid for it at start, as Claude Code does. It reads the `--settings` hooks document the daemon injects: on `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` it runs the configured hook commands with the event JSON on stdin.

It also loads hooks from the user layer, `<CLAUDE_CONFIG_DIR>/settings.json`, and runs that layer's groups before the `--settings` groups for each event, as Claude Code merges its layers; a missing or malformed user file leaves the layer empty. A missing one is silent on the screen, as under the real CLI: its `user-settings-missing <path>` line goes to `fake-claude.log` in the OS temp dir; a malformed one prints `user-settings-invalid <path>`.

Every start runs `SessionStart` right after the banner, with `session_id` the conversation id and `source` `resume` when started with `--resume`, else `startup`; the source is the matcher's subject, so a `SessionStart` group with a `matcher` (`startup`, `resume`, `startup|resume`) runs as under the real CLI.

The prompt cue leaves its turn open under the spinner, the ask cue paints the question dialog and then runs `Stop` 5 s later and paints the idle box, and the bg cue ends its turn with a `Stop` that lists a running background shell (the payloads are in `index.mjs`).

The prompt box echoes what is typed or pasted into it as `❯ <text>`, cut to one row, the way Claude's box shows a draft, so the daemon's chat send sees its message in the box before it presses Enter. Paste markers and control bytes are left out, and nothing is echoed while an EOT-terminated paste is buffered.

Any other submitted line that is not blank and does not start with `/` is a message, and it runs a turn the way the CLI does. The turn writes the user record and runs `UserPromptSubmit`, then paints the spinner. It ends with the assistant's answer (`Done: <text>`), a `system` `turn_duration` record, `Stop`, and the idle box under an `answer:` line.

A message containing `rt-perm` asks a permission before it answers. It writes an assistant `tool_use` record for `Bash` `echo rt-fake` and runs `PermissionRequest`, then paints Claude's permission dialog in place of the prompt box: `Do you want to proceed?` over `1. Yes`, `2. Yes, and don't ask again for: …` and `3. No`, with the `Esc to cancel · Tab to amend` footer.

The digit typed at that dialog, with no Enter, answers it. `1` or `2` writes the `tool_result`, runs `PostToolUse` and ends the turn answering `Ran echo rt-fake and it printed rt-fake.`. `3` writes an error `tool_result` and an interrupted-by-user record, then runs `Stop`.

The transcript goes where the daemon's chat mirror reads it: `<CLAUDE_CONFIG_DIR>/projects/<encoded working directory>/<id>.jsonl`, one JSON record per line, chained by `parentUuid`. The `<id>` comes from the `--session-id` or `--resume` argument, else the uuid the run minted. The folder name is the working directory with every character outside `[A-Za-z0-9-]` turned into `-`. A run with no `CLAUDE_CONFIG_DIR` writes no transcript.

## `auth login`

Run as `auth login …` (further arguments, such as `--claudeai`, are accepted and ignored), the shim stands in for `claude auth login` instead of a session: it prints the CLI's lines and ends without a banner, hooks or a prompt.

The `FAKE_CLAUDE_LOGIN` knob picks the outcome:

- unset, empty or `ok`: prints `Opening the browser to sign in… (fake)`, writes the login, then prints `Login successful. (fake)` and exits 0; the success line comes only after both files are in place.
- `fail`: prints `fake-claude: Login failed. (fake)` on stderr, writes nothing, exits 1.
- `hang`: prints the opening line, writes nothing and waits until it is killed.
- anything else: prints `fake-claude: unknown FAKE_CLAUDE_LOGIN value: <value>` on stderr and exits 2.

The login goes into `CLAUDE_CONFIG_DIR`, which the shim creates when it is missing; without the variable it prints `fake-claude: auth login needs CLAUDE_CONFIG_DIR` on stderr and exits 2. `.claude.json` is written first, then `.credentials.json`, both whole through a temp file and a rename, each ending in a newline:

- `.credentials.json`: a `claudeAiOauth` object with `fake-access-<tag>` and `fake-refresh-<tag>` tokens, an expiry eight hours out, the `user:inference` and `user:profile` scopes, and the `max` subscription type.
- `.claude.json`: an `oauthAccount` object with the email, `Fake org`, and the derived `accountUuid` and `organizationUuid`.

The email is `FAKE_CLAUDE_LOGIN_EMAIL`, else `login@example.com`; `<tag>` is the first 12 hex digits of its SHA-256, and the two uuids are the first 32 and the next 32 hex digits of that digest in uuid shape, so the same email is the same identity and two emails differ.

`.claude.json` keeps every key a file already there holds and sets `oauthAccount`. A file that exists but is not a JSON object fails the login with `fake-claude: <path> is not a JSON object` on stderr and exit 2, writing nothing.

## Headless stream-json run

With `FAKE_CLAUDE_STREAM_JSON` set to a positive whole number of milliseconds, the shim stands in for a headless `claude -p --output-format stream-json` run that never finishes, as a dispatch run's child: it looks at no argument, prints no banner and runs no hook. It writes to stdout, each line ended by `\n`:

- at once, `{"type":"system","subtype":"init","session_id":"00000000-0000-4000-8000-00000000f00d"}`;
- then one line every interval, cycling through `{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}`, the same with `"name":"Edit"`, and `{"type":"assistant","message":{"content":[{"type":"text","text":"step <n>"}]}}` with `n` counting up from 1.

It never writes a `result` line and never exits on its own: it waits to be killed. Any other non-empty value prints `fake-claude: FAKE_CLAUDE_STREAM_JSON must be a positive whole number of milliseconds: <value>` on stderr and exits 2. The live tier's dispatch spec (`apps/native/tests/e2e_dispatch.rs`) runs it as `node index.mjs` with its stdout in a run's stream file.

A crash is appended to `fake-claude-crash.log` in the OS temp dir.

`fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

The Claude plugin hands this folder to the live tier through `fake_cli` (`crates/agents/claude/src/lib.rs`, feature `test-support`): the harness reads it from every plugin in `agent-registry` and sets the plugin's program variable, `RUSTLING_TULIP_CLAUDE` here, to the shim's path, and the daemon resolves the CLI from that variable at spawn time. Node must be on `PATH`.
