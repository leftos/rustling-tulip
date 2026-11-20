# Spike: does a command sent into Claude Code's prompt run, and which commands open an overlay

The chat composer sends text into the agent's prompt as one bracketed paste followed by Enter once the screen shows the paste (`type_message` and `bracketed_paste` in `crates/daemon/src/inject.rs`). This spike measured whether a slash command delivered that way runs as a command, and which built-in commands open an overlay, picker or dialog that the chat cannot show, for the command catalog's "opens in the terminal" flag.

## Setup

- Claude Code 2.1.327 (`claude --version`), the real `claude` on PATH, interactive session with its default options.
- An isolated daemon built from this tree (`cargo build -p daemon -p tracer`, debug), started with its config, binaries, worktrees, tailnet, push, dispatch, gates and accounts dirs all under a `.tmp` scratch folder and `RUSTLING_TULIP_ACCOUNTS_OFFLINE=1`, as `docs/daemon-cli.md` "Isolating an experiment" describes.
- Driven with the session-driving verbs: `spawn --agent claude --cwd <empty scratch folder>`, `send <id> <text>` (the composer path: paste, then Enter), `send <id> --keys <spec>` (typed keys), `read`, `wait --status idle` and `kill`.
- The scratch folder sat inside a checkout with a project `.mcp.json`, so Claude Code first showed its "New MCP server found in this project" prompt; it was answered with its default row, "Continue without using this MCP server".
- One short message ("Reply with the single word ok.") gave `/compact` a conversation to compact.
- A real `claude` session shares the user's `~/.claude/settings.json`, which isolating the daemon's own folders does not cover: a typed `/model <name>`, `/effort <level>` (all but `max` and `status`), Enter in the `/model` or `/effort` picker and `/config` all write it. A spike that sends them changes the user's defaults for every new session.

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

## Model and effort commands

Measured on Claude Code 2.1.328 with the Setup above: an isolated daemon, the real `claude`, a session that started on the account's default model with the owner's saved effort, driven with `send <id> <text>` (paste then Enter) and `send <id> --keys <spec>`. At start the banner read `Opus 5.5 with high effort · Claude Ultra`, and a right-aligned indicator above the prompt read `● high · /effort`. The status line named the model (`Opus 5.5`).

### Headlines

- **A. `/model <name>` switches at once.** It opens no picker and starts no model turn, and the text never reaches the model. `send <id> /model opus` echoed `❯ /model opus` and `⎿  Set model to Opus 5.5 and saved as your default for new sessions`. The full id `/model claude-opus-5-5` printed the same row. `/model default` printed `⎿  Set model to Opus 5.5 (default) and saved as your default for new sessions`.
- **B. Every typed `/model <name>` writes the user's `~/.claude/settings.json`.** `/model opus` appended `"model": "opus"` to it, the full id replaced that with `"model": "claude-opus-5-5"`, and `/model default` removed the key. Claude Code's docs say the same: typed `/model <name>` behaves like Enter in the picker, which saves the default; only `s` in the picker switches for this session alone (https://code.claude.com/docs/en/model-config, "Setting your model").

  So a composer that sends `/model sonnet` changes the user's default model for every new session, not just this one. For that reason `/model sonnet` and `/model haiku` were not sent; Sonnet was reached through the picker's `s` instead (C). Haiku was not switched to at all.
- **C. A session-only switch goes through the picker.** `send <id> /model`, then `--keys "<Down><Down>"` to `❯ 4.  Sonnet 5.5`, then `--keys "s"` printed `⎿  Set model to Sonnet 5.5 for this session only`. The status line changed to `Sonnet 5.5`, the effort indicator to `◐ medium · /effort`, and settings.json was not touched.
- **D. A switch in a conversation with a turn asks first.** Switching back to Opus with `s` after one turn opened a dialog, during which the daemon read the session as `awaiting_input`:

  `Switch model?` / `Your next response will be slower and use more tokens` / `This conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read.` / `❯ 1. Yes, switch to Opus 5.5` / `2. No, go back` / `Enter to confirm · Esc to cancel`.

  A `send <id> /effort high` made while it was open was held back by the composer and never typed; `--keys "<Enter>"` took Yes and printed `⎿  Set model to Opus 5.5 for this session only`. Whether a typed `/model <name>` opens the same dialog was not measured (it would have saved a default).
- **E. The transcript names the new model on the next turn.** After the Sonnet switch and one message ("Reply with the single word ok."), the session's jsonl (`~/.claude/projects/<cwd slug>/<claude session id>.jsonl`) had an `assistant` record with `message.model: "claude-sonnet-5-5"` and `requestedModel: "claude-sonnet-5-5"`. The `api-request` record had `params.model: "claude-sonnet-5-5"`.
- **F. `/effort <level>` sets effort; `/model` takes no effort argument.** The command menu's hints are `/model  [model]` and `/effort  [low|medium|high|xhigh|max|auto|ultracode [on|off]]`. `/model opus high` was not sent: the hint names one argument, and a typed `/model` saves what it is given.
- **G. Main-thread records carry effort.** The main-thread (`isSidechain: false`) `assistant` record had top-level `effort: "medium"` and `perTurnEffort: "medium"`, and the `api-request` record had `params.output_config.effort: "medium"`. No `user` or `system` record carries an effort field.

### The `/model` picker

Bare `/model` (paste then Enter) opens this overlay, the current model ticked and highlighted:

```
  Select model
  Your pick is the default for new sessions. For other models, use --model.
    1.  Default (recommended)  Opus 5.5 · Best for everyday, complex tasks
  ❯ 2.  Opus 5.5 ✔             For complex work and everyday tasks
    3.  Fable 5.2              For your toughest challenges
    4.  Sonnet 5.5             Most efficient for simpler tasks
    5.  Haiku 5.5              Fastest for quick answers
    ▸   More models
  ● High effort ←/→ to adjust
  s to use this session only · Enter to save · Esc to cancel
```

Up and Down move between models. Effort is the row under the list, changed with Left and Right: `○ Low effort`, `◐ Medium effort (default)`, `● High effort`, `◉ xHigh effort`, `◈ Max effort`, stopping at both ends. Enter saves the highlighted model (and effort) as the default, `s` applies it to this session only, and Esc closes it with `⎿  Kept model as Opus 5.5` and saves nothing, even after the effort row was moved.

### Effort commands

| Sent (paste then Enter) | Screen | Saved to settings.json |
|---|---|---|
| `/effort status` on Sonnet 5.5 at its default | `⎿  Effort level: auto (currently medium)` | No |
| `/effort max` | `⎿  Set effort level to max (this session only): Maximum capability with deepest reasoning. …`; indicator `◈ max · /effort` | No |
| `/effort` (bare) | Overlay titled `Effort`: a slider `low  medium  high  xhigh  max` between `Faster` and `Smarter`, `Ultracode  off` / `Tab to toggle`, a cost line (`~3.5x the estimated cost of medium (the default)` on xhigh), and a footer `←/→ to adjust · s to use this session only · Enter to save · Esc to cancel` (on max: `←/→ to adjust · Enter to use this session only · Esc to cancel`) | Esc printed `⎿  Cancelled`; nothing saved |
| `/effort status` on Opus 5.5 after the `max` above and a switch to Opus | `⎿  Current effort level: max (Maximum capability with deepest reasoning. …)` | No |
| `/effort high` on Opus 5.5 | `⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation`; indicator `● high · /effort` | Yes: the file was rewritten. Its Opus level was already `high`, so only key order changed |

A session-only effort (`max`) stays across a session-only model switch: set on Sonnet, it still read `max` on Opus. A switch from Opus at its saved `high` to Sonnet, which had no saved level, showed Sonnet at `auto (currently medium)`. The docs say a typed `/effort low|medium|high|xhigh` saves the level for the active model under `modelSettings`, and `max` is session-only (https://code.claude.com/docs/en/settings-reference, `effortLevel`).

### The transcript's record of a command

Each command run from the prompt leaves `user` records in the jsonl: `<command-name>/model</command-name>` with `<command-args>opus</command-args>`, then ``<local-command-stdout>Set model to `Opus 5.5` and saved as your default for new sessions</local-command-stdout>``. The `/effort` commands are recorded the same way.

The first bare `/model` closed with Esc was recorded as `system` records (subtype `local_command`) instead, with ``<local-command-stdout>Kept model as `Opus 5.5`</local-command-stdout>``.

### For the catalog and the composer

- Sending `/model <name>` or `/effort low|medium|high|xhigh` through the composer runs it, and also saves it as the user's default for new sessions. A chat control meant to switch one session should either say so, or drive the picker (`/model`, arrows, `s`) as typed keys, which needs the `Switch model?` dialog handled once the conversation has a turn.
- `/effort max` and `/effort status` are safe for one session from the composer. Bare `/model` and bare `/effort` are overlays ("opens in the terminal").
- The chat can read the active model from the next `assistant` record's `message.model`, and the effort from its top-level `effort`.

### The picker's session-only paths on 2.1.329

Measured on Claude Code 2.1.329 with the same Setup, at 120 by 32: every path below goes through the picker's `s`, never Enter, and none of them wrote `settings.json` (its hash was checked after each step). The screens are fixtures in `crates/daemon/src/screen/fixtures/`, named `claude--v2.1.329-model-*.txt` and `claude--v2.1.329-effort-status.txt`.

- **The picker.** Bare `/model` draws the rows of the earlier section, with the tick on `1.  Default (recommended) ✔` when `settings.json` names no model, a `▸   More models` row, the effort row and the footer `s to use this session only · Enter to save · Esc to cancel` (`claude--v2.1.329-model-picker.txt`). The daemon reads the session as `idle` while the picker is open (`working` over a running turn), so the picker is found by its `Select model` row, never by status.
- **The effort row follows the cursor**: on Sonnet it reads `◐ Medium effort (default) ←/→ to adjust` (`…-model-picker-on-sonnet.txt`).
- **`s` on another row** prints `⎿  Set model to Sonnet 5.5 for this session only`, and the status line, the effort indicator and the banner follow the new model (`…-model-session-only-switch.txt`). Reopened, the picker has the tick and the cursor on the session's model (`…-model-picker-after-session-switch.txt`).
- **`s` on the ticked row** opens no dialog and prints the same line again (`…-model-s-on-current.txt`), also after a turn.
- **`s` with the effort row moved** adds the level: `⎿  Set model to Opus 5.5 for this session only with xhigh effort`, and the indicator reads `◉ xhigh · /effort` (`…-model-picker-effort-right.txt`, `…-model-effort-right-s.txt`; Left: `…-effort-left*.txt`). `/effort status` then reports the level (`claude--v2.1.329-effort-status.txt`).
- **The `Switch model?` dialog** opens for `s` on another model after a turn, and the daemon reads it as `awaiting_input` (`…-model-switch-dialog.txt`). `2. No, go back` returns to the picker with the cursor back on the ticked row, not the row it left (`…-model-switch-dialog-go-back.txt`); `1. Yes, switch to …` prints the session-only line (`…-model-switch-dialog-yes.txt`).
- **More models** hides the effort row and changes the footer to `Enter to expand · Esc to cancel`, with no `s`; Right does nothing there (`…-model-picker-more-models*.txt`).
- **Esc** prints `⎿  Kept model as Sonnet 5.5` and saves nothing, even with the cursor and effort moved (`…-model-picker-before-esc.txt`, `…-model-esc.txt`). Over a running turn the close shows as a notice beside the prompt instead (`…-model-typed-during-turn-esc.txt`).
- **Over a running turn**, `/model` typed as keys opens the picker at once above the spinner, and the turn finishes underneath it (`…-model-typed-during-turn-0.7s.txt`, `…-2.7s.txt`). A composer `send` of `/model` made in the turn's first second never reached the prompt and left no log line (`…-model-during-turn-1s.txt`, `…-3s.txt`).

**The transcript.** Each `s` switch appends four records: a `file-history-snapshot`, a meta `user` record holding the local-command caveat, a `user` record `<command-name>/model</command-name>` with empty `<command-args>`, and a `user` record `<local-command-stdout>Set model to \`Sonnet 5.5\` for this session only</local-command-stdout>` (with `with \`xhigh\` effort` after it when the effort moved).

An Esc close is two `system` records of subtype `local_command`, the second `<local-command-stdout>Kept model as \`Sonnet 5.5\`</local-command-stdout>`. The main-thread replies after a switch name the new model and its effort.

### Not measured

- Typed `/model sonnet`, `/model haiku`, and `/model opus high`, because each would write the owner's default model. Whether a typed `/model <name>` in a conversation with a turn opens the `Switch model?` dialog, and whether it saves before or after that dialog.
- Switching to Haiku 5.5 or Fable 5.2, and the models behind `More models`.
- `/effort low|medium|xhigh|auto` and `/effort ultracode`, and the picker's Enter path, all of which save.
- Whether the daemon's own chat mirror shows the switched model: `sessions --json` carries no model field.
