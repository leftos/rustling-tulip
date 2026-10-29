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

## The agent in view (CV.23)

What Claude Code's terminal shows while a subagent's transcript is in view, and how to leave and come back, for CV.24 of [conversation-view.md](../plans/conversation-view.md). The screens are fixtures `v160`–`v172` in `crates/daemon/src/screen/fixtures/`.

### Setup

- Claude Code 2.1.312, Opus 5.5, vanilla `claude` with the user's settings (default mode `auto`), in herdr panes of 152 × 72.
- Two or three background general-purpose subagents kept busy by a foreground `python -c "import time; time.sleep(580)"` (the user's hooks refuse a plain `sleep`).
- The built-in Heads up raised no notice on four labs (b, c, e, g) in these sessions; on lab c the main agent named the hazard itself.
- The Heads up rows were measured with a spike mod instead: a band above the prompt with the built-in's three keys, raised by a slash command after a delay, whose `2: Chat in main session` makes the built-in's own call, `$.prompt.fill({ text: "…\n", mode: "replace" })`. The built-in is a plugin on the same mod API, so the band and the fill behave as the built-in's do; its look does not match.

### What the screen shows

- **Main view with agents running** (`v160`): the agents panel under the mode line, `● main` first, then one row per agent, `◯ general-purpose  <description or current activity>  <time> · ↓ <tokens>`; an agent started by another agent is drawn under it as `└ ◯`. The filled dot marks the view in force. While the box is empty the mode line ends `← N agents`.
- **A subagent in view** (`v161`, `v162`): the transcript above the box is the subagent's own, starting with its prompt as a `❯` row; the rule over the box carries the agent's description at its right end (`─ Agent B python wait ─`); the empty box reads `Message @general-purpose…` (the agent's type); the panel's filled dot moves to the agent's row. These three are the reader's evidence; the description alone is not unique.
- **Focus**: a `❯` at the start of a panel row is the panel's selection and stays after the keys leave the panel. Only the hint line says who has the keys: `↑/↓ to select …` or `Enter to view · x to stop …` for the panel, the mode line (`⏵⏵ auto mode on …`) for the box.
- **AskUserQuestion with a subagent in view** (`v167`): the dialog draws in the subagent's view, under the subagent's transcript, in place of the box, with the rule naming the agent still the last row. Down moves the dialog's cursor (`v172`); the panel cannot be reached while the dialog holds the keys.
- **Heads up with a subagent in view** (`v169`): the band above the prompt draws in the subagent's view, and a band's hook sees the agent as the view in force.

### Keys

| Move | Keys | Notes |
|---|---|---|
| Subagent's view → main | Down until the hint line reads `↑/↓ to select`, Enter, Up | Down lands on `main` first. Down took two presses after a stash, a submit or a history recall (the first only drops `← N agents`), and one press with a one- or two-row draft (`v164`, `v170`), so each Down is checked against the hint line; at most three were needed. Enter shows main with the panel still holding the keys; Up from the `main` row gives them to the box. |
| Main → a subagent's view | Down until `↑/↓ to select`, Down once per row to the agent's row, Enter, then Up until the mode line shows | The panel keeps the keys after Enter, on the agent's row; Up walks the rows and leaves the panel from `main`. Rows change as agents start, finish and nest (an interrupted, finished agent's row was cleared), so the row is found by the agent's description, never by count. |
| Never | Esc, Left, Enter on an empty box, extra Up | Esc on an empty box in a subagent's view interrupts the subagent's running tool (`Interrupted · What should Claude do instead?`, `v163`) and stays in the view. Left opens `Background this session?` with `Background anyway (tasks will be stopped)` selected (`v168`; its Esc cancels it). Enter on an empty box opened the Background tasks list (`Esc to close`). Up in the box recalls the view's own prompt history (`─── History 1/1 ───` in the rule; a subagent's history is its prompt), and the next Down or Enter then acts on the recalled text. |

### Drafts

- **One box for every view.** A draft typed with a subagent in view was still in the box after switching to main (`v164` → `v165`), and the `Chat in main session` draft filled in a subagent's view came along the same way (`v170` → `v171`). Claude Code keeps no draft per view: Enter sends the box to whichever view is up.
- **The misroute, seen**: text left in the box with Agent B in view, followed by an Enter, reached Agent B as a queued message (`· 1 queued` on its row), and Agent B answered it. A prompt typed for the main agent while Agent B was in view started work in Agent B (it launched an agent of its own).
- **Chat in main session from a subagent's view** fills the box there (`v170`): with no switch, its Enter goes to the subagent. The fill took the box at once (`isFilled`); the built-in reads the box first and leaves a box holding text alone (`$.prompt.read()` before the fill, in its code).
- **Stash**: ctrl+s (`chat:stash`) on a box holding text stashes it (`› stashed` over the rule's right end; the box empties); on an empty box it puts the stash back. Round trip measured: stash with Agent B in view, switch to main (its box empty, `v160`), back to Agent B, ctrl+s: `draft two for B` came back. A `[Pasted text #1 +34 lines] tail words` draft (`v166`) came back as the same placeholder (the stash keeps `pastedContents`, in the binary); its 35 lines were not re-sent to check them.
- **Clearing**: ctrl+u deletes one line per press with the cursor at the end, so a draft of n rows takes n presses; ctrl+l (`chat:clearInput` in the binary) left the box as it was here; Esc Esc is ruled out above (Rewind on an empty box, an interrupt in a subagent's view).

### Answers

- An AskUserQuestion answered with a digit while a subagent was in view reached the main agent's tool (`Pick a colour → Red`, then the main agent's `Red`), and the view stayed on the subagent.

### Not measured

- The built-in Heads up itself in a subagent's view, and its expanded body: no notice came (above); the look-alike stands in for the band and the fill.
- An image in the box through a stash: pasting one needs an image on the user's clipboard, which the spike did not overwrite.
- A stash made when the stash already holds one (whether the second replaces the first).
- `Chat about this` from a dialog answered in a subagent's view, and where the text typed after it goes; by the one-box rule above it goes to the view in force.

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
