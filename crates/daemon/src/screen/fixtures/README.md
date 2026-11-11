# Screen fixtures

The spike captures (`v03-ready.txt`, `c11-turn1-end.txt`, `p98-compacted.txt` and the rest of the `v*`/`c*`/`p*` names) are Claude Code 2.1.307 screens captured for `docs/spikes/chat-mirror-tui.md`, plain text with no escape sequences.

`v160-…` to `v172-…` are Claude Code 2.1.312 screens (152 columns) captured for the "The agent in view (CV.23)" section of `docs/spikes/chat-mirror-tui.md`: the main view with the agents panel, a subagent's transcript in view, and drafts and prompts across a switch. `v169`, `v170` and `v171` show a Heads up look-alike drawn by a spike mod, not the built-in notice: its band and keys differ in look, while its draft lands through the same `prompt.fill` call.

`v173-…` to `v200-…` are Claude Code 2.1.314 screens (152 columns) captured for CV.24: the built-in Heads up in a subagent's view, the prompt stash (`› stashed`, one slot shared by every view), an image through the stash, the tasks pill holding the keys, and AskUserQuestion's Chat about this from a subagent's view.

`v2318-agent-view-subagent-running.txt` and `v2318-agent-view-subagent-finished-main-woken.txt` are Claude Code 2.1.318 screens (120 columns) taken for RT-946: a subagent's view with its spinner up while the main agent's turn waits, and the same view with its idle box once the subagent's report has woken the main agent.

`v2318-subagent-running-w40.txt` is the panel's running-subagent row at 40 columns, as Claude Code 2.1.318 draws it with no description: `  ◯ general-purpose 2m 7s · ↓ 66.5k`.

`v2319-ask-long-question-w120.txt` is a question over 80 characters at 120 columns, as Claude Code 2.1.319 draws it: a `│` gutter on each of the question's rows. A question fixture under 80 characters has no gutter, so a change to question reading needs this one too.

`v2322-ask-vim-*-w120.txt` are Claude Code 2.1.322 screens (120 columns) taken for RT-1442 with the vim editor mode on, one question dialog in turn: the cursor on option 1 (`first-option`, footer `Enter to select · ↑/↓ to navigate · Esc to cancel`), on `Type something` (`insert`, footer `-- INSERT -- · Enter to select · ↑/↓ to navigate · ctrl+g to edit in Notepad`, no `Esc to cancel`), after typing `x` there (`insert-typed`), after one Esc (`normal`, footer `Enter to select · ↑/↓ to navigate · ctrl+g to edit in Notepad · Esc to cancel`), and after a second Esc cancelled the dialog (`cancelled-idle`, the idle box's status line `  -- INSERT -- ⏵⏵ auto mode on …`).

`v152-heads-up-collapsed.txt` is the one capture made by hand: `v152-learn-more.txt` with the expanded title and bullet rows taken out and the key row replaced by the collapsed notice's `1: Learn more   2: Knew this already   0: Dismiss`, since no capture of the collapsed notice exists.

`v03-ready-quoting-question-footer--synthesized.txt` is `v03-ready.txt` with a user prompt and a reply quoting a question's footer (`Enter to select · ↑/↓ to navigate · Esc to cancel`) written into the blank rows above the idle box: a quoted footer is transcript, not a dialog.

`v42-ask-review-cursor-on-cancel--synthesized.txt` is `v42-ask-review.txt` with the cursor moved from `1. Submit answers` to `2. Cancel`, so the review's matched row sits above the cursor.

The `claude--*` files are the `web/src/fixtures/panes/` panes from collie, https://github.com/leftos/collie at commit 82c6bba7, SGR-coloured text under the MIT licence (see `LICENSE-collie` beside them). The exception is `claude--mcp-server-prompt.txt`, a plain-text capture of Claude Code's new-MCP-server dialog taken in this repo.

`screen/rules.rs` and `screen/dialogs.rs` load each file with `include_str!` as one screen (`screen/fixture_screen.rs`): its lines are the rows, fed to a grid wide enough for the widest of them.

`every_fixture_reads_its_state` pins the state each one must read as, and `every_fixture_reads_its_dialog` the input box and dialog the chat reads off it; a capture whose state changes upstream is replaced with a fresh one, never reinterpreted by editing the rule table or the parsers.
