# Spike: what the Claude Code TUI shows, for the chat mirror

What a mirror of a live Claude Code session can see on the screen, in the OSC title, in hooks, in the transcript and in the MessageDisplay hook, and how each prompt is answered with keys. Input for the chat mirror (RT-51, `docs/plans/off-orca.md` SV.1–SV.3, the redesign of `docs/plans/conversation-view.md`) and for the steadier screen-based status (RT-74).

Every claim below was seen in a run. The screens are in [chat-mirror-tui/screens/](./chat-mirror-tui/screens/): `<name>.txt` is the plain screen and `<name>.ansi` the styled one, `v…` from vanilla `claude`, `c…` from `cui` / `cui-ask`, `p…` from a control session with no extra hooks. The names below point at them.

## Setup

- Claude Code 2.1.307, Opus 5.5 at high effort, Windows 11, in herdr 0.9.5 panes of 134 × 74.
- **vanilla**: `claude` with the user's settings (default permission mode `auto`).
- **cui**: `claude --dangerously-skip-permissions --plugin-dir X:\dev\ui-themes` (bypass, the ui-themes mods). **cui-ask**: the same themes without the skip, so permission prompts show; it is the `cui-ask` function in the user's pwsh profile.
- Every hook event was logged by an async command hook beside the user's own hooks; the screen was sampled every 150–400 ms through `herdr pane read`; the transcript is `~/.claude/projects/<encoded cwd>/<session id>.jsonl`.

## Headlines

1. **Several ways a prompt or turn ends fire no hook at all**: Esc on a permission dialog, a plain "No" on one, Esc on an AskUserQuestion, Esc mid-turn. A hook-driven `AwaitingInput` set by `PermissionRequest` is then never cleared by a hook; only the screen (and later the transcript) shows the turn ended.
2. **Text typed into the PTY while a dialog is open is dropped, and the Enter after it selects the highlighted option.** A chat message sent while a permission dialog was up approved the tool (`c100-typed-into-dialog`, `c101-enter-in-dialog`). The composer must be gated on a screen-level "dialog open" check.
3. **The TUI does not stream answer text.** A 40-line answer appeared in one frame when the message completed (control session `p…` confirms it is not the hook logger). The transcript also gets the text at message end. The **MessageDisplay** hook delivered every line live, about 10 s ahead of the screen, but async calls arrive out of order.
4. **The transcript has no thinking text**: every `thinking` block is `''` with a signature. The screen shows only `thought for 14s`.
5. **Turns start with nobody typing**: each subagent hand-back and each finished background shell starts a main-session turn (`UserPromptSubmit` with `<agent-message …>` or `<task-notification>`); a `!` command makes Claude reply with no `UserPromptSubmit` at all.
6. **Rows above the live area change after the fact**: turn-end lines `… · 1 shell still running` were redrawn without the suffix once the shell exited. A screen tailer cannot treat scrolled rows as final.
7. **Dialog options are only on the screen.** The hook's `permission_suggestions` (`addRules python -c "print(6*7)"`) differ from the dialog's `2. Yes, and don’t ask again for: python *`, and the option count differs per tool (3 for Write/Edit, 4 for Bash).
8. **The OSC title cannot see a dialog**: `◐`/`◑` while working, `✳` both at idle and while a dialog waits (`c120-osc-titles.txt`).
9. **The terminal's input box can already hold text**: the user's own draft, a queued message, or a Heads up notice's "Chat in main session" draft. A mirror that pastes would merge with it.
10. **Prior art exists for all of this**: herdr's declarative screen-state manifest (Apache-2.0) and collie's screen parsers with 194 pane fixtures (MIT). See "Prior art".

## Which source sees what

| Event | Screen | OSC title | Hooks | Transcript |
|---|---|---|---|---|
| Folder trust dialog | `Quick safety check`, `Yes, I trust this folder` | — | none (SessionStart fires only after) | — |
| Turn starts (typed) | prompt row, spinner | `◐`/`◑` | UserPromptSubmit | `user` record |
| Turn starts (agent hand-back, shell done) | `● Agent "…" finished`, `● Background command "…" completed` | `◐`/`◑` | UserPromptSubmit `<agent-message…>` / `<task-notification>` | — |
| Turn starts (`!` command) | `! for shell mode`, output under `⎿` | `◐`/`◑` | **none** | `<bash-input>`, `<bash-stdout>` user records |
| Answer text | whole message at once | — | MessageDisplay per line (out of order), Stop `last_assistant_message` | `assistant` text block at message end |
| Thinking | `thinking with high effort` / `thought for Ns` | — | — | empty `thinking` block |
| Tool call | row with blinking `●` (vanilla) or `$ …` / `◧ …` (cui) | — | PreToolUse, PostToolUse(+Failure), PostToolBatch | `assistant` tool_use, `user` tool_result |
| Permission dialog | `Do you want to …?` + numbered options + `Esc to cancel · Tab to amend` | `✳` | PermissionRequest at once; Notification `permission_prompt` ~6 s later | — until answered |
| Permission denied / Esc | `⎿  Interrupted · What should Claude do instead?` | `✳` | **none** | is_error tool_result + `[Request interrupted by user for tool use]` + `turn_duration` |
| AskUserQuestion | tab bar `←  ☐ … ✔ Submit  →`, options | `✳` | PreToolUse + PermissionRequest; Notification `permission_prompt` 6 s later | answered: tool_result + structured `toolUseResult` |
| AskUserQuestion Esc / "Chat about this" | `● User declined to answer questions` | `✳` | Esc: **none**; Chat about this: PostToolBatch only | — |
| Plan approval | `Ready to code?` … `Would you like to proceed?` | `✳` | PreToolUse + PermissionRequest(ExitPlanMode); Notification "Claude Code needs your approval for the plan" | approved: `● User approved Claude's plan` receipt |
| Message typed mid-turn | dim `❯ text` over the box, `Press up to edit queued messages`, `ctrl+enter to send now` | — | UserPromptSubmit **when delivered**, not when typed | `attachment` `queued_command` |
| Esc mid-turn | `⎿  Interrupted · …` | `✳` | **none** | interrupt text record |
| Subagents running | `● Running 2 agents…` tree; panel under the mode line `◯ general-purpose  <desc>  5s · ↓ 61.1k tokens` | — | SubagentStart/Stop; the subagent's own tool hooks carry `agent_type` | `subagents/agent-<id>.jsonl` |
| Background shell | mode line `· 1 shell`; turn end `· 1 shell still running` | — | PreToolUse/PostToolUse of the launch only | output in `tasks/<id>.output` |
| Tasks | `⎿  ◻ check div`; after the turn `2 tasks (0 done, 2 open)` | — | TaskCreated / TaskCompleted | TaskCreate tool calls |
| Heads up notice | `✦ Heads up · …` / `1: Learn more   2: Knew this already   0: Dismiss` | — | SubagentStop `last_assistant_message` = `learn: <text>` (else `learn: none`) | — |
| Slash command output | under `⎿` | — | — | `system` `local_command` with raw SGR escapes |
| /compact | `Compacting conversation…`, `⎿  Compacted (ctrl+o …)` | — | PreCompact, SessionStart `compact` | `compact_boundary`, `isCompactSummary` |
| Session name | right side of the input's top rule | after the glyph | — | `agent-name`, `ai-title` |
| Permission mode | last row: `⏸ manual mode on`, `⏵⏵ accept edits on`, `⏸ plan mode on`, `⏵⏵ auto mode on`, `⏵⏵ bypass permissions on` | — | every payload's `permission_mode` | `permission-mode` records |

## Screen catalogue

Anchors are the strings a detector keys on; the full screens are in `chat-mirror-tui/screens/`.

### Idle, working, turn end

- **Idle**: the input sits between two full-width `─` rules with `❯ ` (an empty box may show a dim `Try "…"` placeholder); under it the status line and the mode line. A right-aligned `● high · /effort` row sits over the top rule. `v03-ready`, `c03-ready`.
- **Working**: `<glyph> <Verb>… (<details>)`; the glyph cycles `· ✢ * ✶ ✻ ✽` every ~450 ms, the verb is random and can change mid-turn. Details: elapsed, `↓ 1.2k tokens`, `thinking with high effort` / `still thinking with high effort` / `thought for 14s`, `running PreToolUse hooks… 2/3`. cui inserts a phase after the verb: `· responding`, `· tool input`, `· tool use`, `· thinking`. The first ~4 s show no details.
- **Turn end**: vanilla `✻ Baked for 22s · done 5:13 PM` (past-tense verb, duration, clock, optional `· 1 shell still running`); cui `──── Sautéed for 18s ───…` in the model's colour, no clock.
- A `⎿  Tip: …` row can appear under the spinner; a `You've used 86% of your weekly limit · resets …` row can appear over the input.

### Messages and tools

- **User message in scrollback**: vanilla `❯ <text>` with Markdown styling applied; cui no `❯`, a padded block on its own background. A queued message lands inline between tool rows as another `❯` row.
- **Answer**: vanilla starts with `● ` (seen as `⏺ ` too; accept both), headings drawn as bold text without `#`, tables box-drawn; cui draws a rounded `╭─╮ │ │ ╰─╯` box with 2-column padding, `•` bullets, tables without grid lines.
- **Tools**: vanilla shows a live group (`● Reading 1 file…`, `⎿  $ git log --oneline`, `Reading calc.py · 2s`) whose `●` **blinks every frame while it runs, including while a dialog waits**, folding to `Read 1 file, ran 2 shell commands`. cui shows `◧ … Read(calc.py)`, `$ … <description>` / `    Bash(<command>)`, `✎ ✓ ● Write(…)`, folding to chips `[◧ 1 read] [$ 2 ran]`, `[$ 1 stopped ⏸]`, `• ✕`.

### Dialogs

All replace the input box and the status and mode lines; the task list (when there is one) is drawn below them.

- **Permission** (`v21-perm-bash`, `c22-perm`, `c52-edit-perm`): `─` rule, kind (`Create file`, `Edit file`, `Bash command`), optional `Tip: …`, subject (path or the Bash description), a `╌` box with the content (file preview, numbered diff `  9 +def div(a, b):`, or the command, long commands with `│ ` gutters), `This command requires approval`, `Do you want to proceed?` / `Do you want to create notes.txt?`, numbered options with `❯`, footer `Esc to cancel · Tab to amend`.
- **AskUserQuestion** (`v31-ask-q1` … `v43-ask-submitted`): tab bar `←  ☐ Next fn  ☐ Tests  ☐ Style  ✔ Submit  →` (`☒` once answered), question, options as a label row plus an indented description row, `N. Type something.`, a `─` rule, `N+1. Chat about this`, footer `Enter to select · Tab/Arrow keys to navigate · Esc to cancel`. Multi-select rows read `1. [ ] add` / `[✔]` and end with an unnumbered `Next` row.

  A question with previews lists options on the left and a `┌─┐` preview box on the right, adds `Notes: press n to add notes`, has no `Type something`, and its footer reads `Enter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions · Esc to cancel`.

  After the last question: `Review your answers` / ` ● <question>` / `   → <answer>` / `Ready to submit your answers?` / `1. Submit answers` / `2. Cancel`. cui indents the card by 2 columns and its full-width rules wrap (`──` on the next row).
- **Plan approval** (`v51-plan-dialog`): `▔▔▔` row, `─` rule, `Ready to code?`, `Here is Claude's plan:`, a `╌` box with the plan as Markdown, `Claude has written up a plan and is ready to execute. Would you like to proceed?`, `1. Yes, and use auto mode` / `2. Yes, manually approve edits` / `3. Tell Claude what to change` with `shift+tab to approve with this feedback`, footer `ctrl+g to edit in Notepad · ~\.claude\plans\<file>.md`.
- **Folder trust** (`v01-start`): `Accessing workspace:`, `Quick safety check: …`, `❯ No, exit` (the default) / `Yes, I trust this folder`, `Enter to confirm · Esc to cancel`.
- **Heads up notice** (`v152-learn-more`, `c152-knew`): over the input's top rule, a right-aligned `[-]` row, then `✦ Heads up · <notice>` / `1: Learn more   2: Knew this already   0: Dismiss`. Learn more expands it in place (a bold title, bullets) with `1: Understood   2: Chat in main session   0: Dismiss`.
- **Screen-only overlays, no hook**: Rewind (Esc Esc on an empty box; `v95-rewind`), the detailed transcript (ctrl+o; `Showing detailed transcript · ctrl+o to toggle …`, `v96-ctrl-o`), `/model` (`Select model` … `Enter to set as default · s to use this session only · Esc to cancel`, `v140-model-picker`), `/btw` (`↑/↓ to scroll · c to copy · f to fork · Esc to close`, `v141-btw`), the slash-command popup while typing `/`.

## Answering with keys

| Dialog | Keys seen to work |
|---|---|
| Folder trust | Down, Enter. Digits do nothing. |
| Permission | A digit answers at once (no Enter). Tab turns option 1 into a field (`Yes, and tell Claude what to do next`); typed text replaces it (`Yes, but print 6*8 instead`), Enter sends: the tool runs and the text arrives as a user message. Esc and the "No" option interrupt the turn. |
| AskUserQuestion, single choice | A digit selects and moves to the next question. |
| AskUserQuestion, multi-select | Digits toggle without moving the cursor; Enter on `Next` moves on. Tab moved the cursor down one row here. |
| AskUserQuestion, Type something | A digit only ticks it; text is taken only while the cursor is on that row (footer adds `ctrl+g to edit in Notepad`). |
| AskUserQuestion, previews | Up/Down change the preview; `n` opens a note; Tab switches question. |
| Review | `1` submits, `2` cancels. |
| Plan approval | `1`/`2` approve; `3` turns option 3 into a field, Enter sends it as feedback (Claude revises and asks again); shift+tab approves with the feedback. |
| Heads up | A bare digit answers (collie measured within 0.5 s); the notice survives one prompt typed past it and clears on the second. |
| Input box | Bracketed paste of a few lines stays inline; 30 lines collapse to `[Pasted text #1 +29 lines]` (`paste again to expand`; the transcript wraps it in `<pasted_content id=…>`). ctrl+u clears one line; Esc Esc clears a draft but **opens Rewind on an empty box**; ctrl+c on an empty box arms exit. |

## What the transcript adds and lacks

- One `assistant` record per content block; many non-message records (`mode`, `permission-mode`, `ai-title`, `agent-name`, `api-request*`, `file-history-*`, `attachment` of many types); timestamps out of order across attachments.
- AskUserQuestion answers arrive structured in the tool_result record's `toolUseResult`: `answers` keyed by question text (multi-select joined with `, `, Other's text inline), `annotations{question:{preview, notes}}`, and `permissionDecision`.
- A permission amend is an extra text block in the same `user` record as the tool_result; a deny is an is_error tool_result plus `[Request interrupted by user for tool use]`.
- Slash-command stdout keeps its ANSI escapes; `!` commands are `<bash-input>` / `<bash-stdout>` user records; long pastes are `<pasted_content>`.
- No thinking text, no notices, no dialog options.

## cui versus vanilla

Hooks and the transcript are identical; the screen differs in the user message (no `❯`, own background), the answer (box, no `●`), tool rows (icons, description first, chips), the spinner (phase suffix), the turn-end line (rule, no clock) and the question card (2-column indent, wrapped rules). Every dialog's text and keys are Claude Code's own in both. A detector that keys on dialog footers and the input box works for both; one that keys on `●` answers or `❯` user rows does not.

## Defects found

- `crates/daemon/src/pty_state.rs:540-556` (`matches_prompt`) reads the last 2 KB of raw output: `has_numbered && has_chevron` is true for any "1." + "2." + "❯", and Claude's idle input box always draws `❯`, a numbered answer has "1." and "2.", and the startup banner `v2.1.307` holds both.

  A Claude session without hooks reads `AwaitingInput` at idle after such output. The blinking `●` also keeps bytes flowing during a dialog, so a byte tail is a poor stand-in for the screen. Booked as RT-264, beside RT-74; the screen engine (`crates/daemon/src/screen/rules.rs`, `docs/plans/screen-status.md`) replaced `matches_prompt` for Claude sessions.
- ui-themes: `/context`'s command card fails (`ui.render hook skipped: … a text child holds a control character`) because the stdout carries SGR escapes; sent to its inbox.

## Prior art

- **herdr** (herdrdev/herdr, Apache-2.0): Claude state from the screen through a declarative manifest (`%APPDATA%\herdr\agent-detection\claude.toml`; upstream plus the user's overlay `~/.claude/tools/herdr-claude-overlay/claude-rules.toml`).

  Each rule has a `state` (`working`, `blocked`, `idle`, `unknown`), a `priority`, a `region` (`osc_title`, `osc_progress`, `prompt_box_body`, `after_last_horizontal_rule`, `bottom_non_empty_lines(N)`, `last_non_empty_above_prompt_box`, `whole_recent`) and `contains` / `line_regex` / `regex` / `any` / `all` / `not` tests.

  `herdr agent explain <pane> --json` prints each rule's evidence. Through this spike it read `working → blocked → done` correctly across an Esc that fired no hook. It also knows states not hit here: `Waiting for N background agents to finish`, `N MCP tasks still running`, MCP elicitation, `Run a dynamic workflow?`, `skip interview and plan immediately`.
- **collie** (AltanS/collie, MIT; the herdr phone client the user contributes to): screen-text parsers for every Claude dialog in `web/src/lib/harness/claude/` (`prompt-select.ts`, `multi-select.ts`, `preview-select.ts`, `paste.ts`, `resume.ts`, `effort.ts`, `switch-model.ts`, `wizard.ts`, `markers.ts`, `chrome.ts`, `above-prompt-notice.ts`, `task-checklist.ts`), tested against 194 pane fixtures `web/src/fixtures/panes/claude--v<version>-<state>--w<width>.txt`.

  Its notes (`X:\dev\_ext\_notes\AltanS\collie\live-checks.md`) hold the recipe that raised the Heads up notice here.

## Owner rulings

Asked with the mock [canvases/chat-mirror-tui](../plans/canvases/chat-mirror-tui/index.html); recorded on RT-51.

- **Keep the mirror and add a screen engine**: state from a herdr-style rule manifest over a rendered screen, dialog options from collie-style parsers, history from the transcript, live text from MessageDisplay. Not the stdio stream peer Claude Desktop uses, and not a hybrid.
- **Composer during a terminal dialog**: the typed message rides on the answer, as "Yes, and tell Claude this" (Tab-amend) or "No, and tell Claude this" (deny, then send). The chat never types a message into the PTY while a dialog is up.
- **Live answers** stream from MessageDisplay, reordered by `index`, ahead of the terminal.
- **Turns nobody typed** show as quiet centred system rows; the **Heads up notice** is pinned over the composer with its keys until answered.
