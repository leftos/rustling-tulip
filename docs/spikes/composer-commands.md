# Spike: does a command sent into Claude Code's prompt run, and which commands open an overlay

The chat composer sends text into the agent's prompt as one bracketed paste followed by Enter once the screen shows the paste (`type_message` and `bracketed_paste` in `crates/daemon/src/inject.rs`). This spike measured whether a slash command delivered that way runs as a command, and which built-in commands open an overlay, picker or dialog that the chat cannot show, for the command catalog's "opens in the terminal" flag.

## Setup

- Claude Code 2.1.327 (`claude --version`), the real `claude` on PATH, interactive session with its default options.
- An isolated daemon built from this tree (`cargo build -p daemon -p tracer`, debug), started with its config, binaries, worktrees, tailnet, push, dispatch, gates and accounts dirs all under a `.tmp` scratch folder and `RUSTLING_TULIP_ACCOUNTS_OFFLINE=1`, as `docs/daemon-cli.md` "Isolating an experiment" describes.
- Driven with the session-driving verbs: `spawn --agent claude --cwd <empty scratch folder>`, `send <id> <text>` (the composer path: paste, then Enter), `send <id> --keys <spec>` (typed keys), `read`, `wait --status idle` and `kill`.
- The scratch folder sat inside a checkout with a project `.mcp.json`, so Claude Code first showed its "New MCP server found in this project" prompt; it was answered with its default row, "Continue without using this MCP server".
- One short message ("Reply with the single word ok.") gave `/compact` a conversation to compact.

Gotcha for anyone driving the verbs from Git Bash: MSYS path conversion rewrites an argument such as `/compact` into a Windows path (`C:/Program Files/Git/compact`) before `rustling-tulipd` sees it, and that text reaches the model as a message. Set `MSYS_NO_PATHCONV=1` (and `MSYS2_ARG_CONV_EXCL=*`), or run the verbs from PowerShell.

## Headlines

- **A. Paste then Enter runs the command.** `send <id> /compact` echoed `❯ /compact`, showed `Compacting conversation…`, and settled on `⎿  Compacted (ctrl+o to see full summary)`. The prompt was empty afterwards and no model message was sent.
- **B. Typed keys run it too.** `send <id> --keys "/compact<Enter>"` gave the same three rows: `❯ /compact`, `Compacting conversation…`, `Compacted (ctrl+o to see full summary)`.
- **C. Not needed**, since A ran. Typing a slash name without Enter (seen for `/review` and `/statusline`) opens the command menu above the prompt with the best match highlighted.
- **D. A skill command reaches the command resolver both ways.** `/keybindings-help` is a skill only the model may invoke in this version, so both `send <id> /keybindings-help` and `--keys "/keybindings-help<Enter>"` printed `This skill can only be invoked by Claude, not directly by users. Ask Claude to use the "keybindings-help" skill for you.` and started no model turn.

  Neither path delivered the text to the model as a message, so the TUI treats a pasted skill name as a command; a user-invocable skill running from a paste is not measured (below).
- **Recommendation: send a picked command as paste then Enter**, through the existing `type_message` path. No `type_command` typed-keys path is needed for a command to run.

  Send the command's canonical name, never an alias or prefix: the menu highlights its best match and Enter takes the highlighted row, so typed `/review` stands on `/code-review (review)`.

## Overlay commands

Each command was typed as `/<name><Enter>` in an idle session, read, then closed with Esc. Every overlay closed with one Esc except `/config`, which took two (the first left its search field). "Title row" is the overlay's first row; a Settings dialog row lists its tabs.

| Command | Opens an overlay or runs inline | Title row |
|---|---|---|
| `/model` | Overlay (picker) | `Select model` |
| `/config` | Overlay (Settings dialog, Config tab, with a search field) | `Settings  Status   Config   Usage   Stats` |
| `/resume` | Overlay (picker, with a search field) | `Resume session` |
| `/agents` | Inline: prints that the wizard is gone | none; prints `The /agents wizard has been removed.` |
| `/mcp` | Overlay | `MCP servers` |
| `/permissions` | Overlay (tabbed) | `Permissions  Recently blocked   Allow   Ask   Deny   Auto mode   Workspace` |
| `/memory` | Overlay | `Memory` |
| `/status` | Overlay (Settings dialog, Status tab) | `Settings  Status   Config   Usage   Stats` |
| `/help` | Overlay (tabbed) | `Help  General   Commands   Custom commands` |
| `/cost` | Overlay (Settings dialog, on the session's estimated cost) | `Settings  Status   Config   Usage   Stats` |
| `/context` | Inline: prints the context usage grid and categories into the transcript | none |
| `/hooks` | Overlay (read-only list) | `Hooks` |
| `/doctor` | Neither: prints `/status checks your setup if Claude can't be reached`, then starts a model turn (spinner, "esc to interrupt"), stopped here with Esc | none |
| `/export` | Overlay (dialog: copy to clipboard or save to file); Esc prints `Export cancelled` | `Export conversation` |
| `/add-dir` | Overlay (dialog with a "Directory path" field); Esc prints `Did not add a working directory.` | `Add directory to workspace` |
| `/todos` | Unknown: `Unknown command: /todos` | none |
| `/review` | Not run: the menu shows it as an alias of `/code-review`, a review that runs model work and reviewer agents | none |
| `/output-style` | Inline: prints `Output style: default` and the available styles | none |
| `/plugin` | Overlay (tabbed) | `Plugins  Discover   Installed   Marketplaces   Errors   Stats` |
| `/statusline` | Not run: the menu row reads `Set up the status line`, a model task that edits the user's settings | none |
| `/usage` | Overlay (Settings dialog, Usage tab) | `Settings  Status   Config   Usage   Stats` |

For the catalog: the overlay rows above are the ones to flag "opens in the terminal". `/doctor`, `/review` and `/statusline` start model work rather than an overlay, and `/agents` and `/todos` no longer do anything useful from a prompt in this version.

## Not measured

- A user-invocable skill (one a user may run, unlike `/keybindings-help`) running its turn from a paste.
- A command with arguments (`/compact <instructions>`, `/model <name>`), and a command sent while the prompt already holds text.
- A command sent while a turn runs: one `/doctor` typed during the running `/doctor` turn was queued and ran after it, but the composer's own busy-lane refusal was not exercised.
- Whether a pasted slash name opens the command menu before Enter, and so whether a pasted alias or prefix resolves to the highlighted row as typed keys do.
- The commands the brief ruled out (`/login`, `/logout`, `/init`, `/clear`, `/exit`, `/theme`, `/vim` and the like), other agents, and Claude Code versions other than the one above.
- macOS and remote (tailnet or SSH) hosts.
