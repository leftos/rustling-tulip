# Screen fixtures

The spike captures (`v03-ready.txt`, `c11-turn1-end.txt`, `p98-compacted.txt` and the rest of the `v*`/`c*`/`p*` names) are Claude Code 2.1.307 screens captured for `docs/spikes/chat-mirror-tui.md`, plain text with no escape sequences; their styled `.ansi` originals are in `docs/spikes/chat-mirror-tui/screens/`.

`v160-…` to `v172-…` are Claude Code 2.1.312 screens (152 columns) captured for the "The agent in view (CV.23)" section of `docs/spikes/chat-mirror-tui.md`: the main view with the agents panel, a subagent's transcript in view, and drafts and prompts across a switch. `v169`, `v170` and `v171` show a Heads up look-alike drawn by a spike mod, not the built-in notice: its band and keys differ in look, while its draft lands through the same `prompt.fill` call.

`v173-…` to `v200-…` are Claude Code 2.1.314 screens (152 columns) captured for CV.24: the built-in Heads up in a subagent's view, the prompt stash (`› stashed`, one slot shared by every view), an image through the stash, the tasks pill holding the keys, and AskUserQuestion's Chat about this from a subagent's view.

`v2318-agent-view-subagent-running.txt` and `v2318-agent-view-subagent-finished-main-woken.txt` are Claude Code 2.1.318 screens (120 columns) taken for RT-946: a subagent's view with its spinner up while the main agent's turn waits, and the same view with its idle box once the subagent's report has woken the main agent.

`v2318-subagent-running-w40.txt` is the panel's running-subagent row at 40 columns, as Claude Code 2.1.318 draws it with no description: `  ◯ general-purpose 2m 7s · ↓ 66.5k`.

`v2319-ask-long-question-w120.txt` is a question over 80 characters at 120 columns, as Claude Code 2.1.319 draws it: a `│` gutter on each of the question's rows. A question fixture under 80 characters has no gutter, so a change to question reading needs this one too.

`v2-ask-wrapped-rule-w297.txt` is a question dialog drawn 2 columns in at 297 columns, whose full-width rules wrap a `──` row under each rule, with a `│` transcript box above it; its transcript and question text are invented.

`v2322-ask-vim-*-w120.txt` are Claude Code 2.1.322 screens (120 columns) taken for RT-1442 with the vim editor mode on, one question dialog in turn: the cursor on option 1 (`first-option`, footer `Enter to select · ↑/↓ to navigate · Esc to cancel`), on `Type something` (`insert`, footer `-- INSERT -- · Enter to select · ↑/↓ to navigate · ctrl+g to edit in Notepad`, no `Esc to cancel`), after typing `x` there (`insert-typed`), after one Esc (`normal`, footer `Enter to select · ↑/↓ to navigate · ctrl+g to edit in Notepad · Esc to cancel`), and after a second Esc cancelled the dialog (`cancelled-idle`, the idle box's status line `  -- INSERT -- ⏵⏵ auto mode on …`).

`v2322-ask-vim-{insert,normal}-w{24,30}.txt` are the same question with the cursor on `Type something` in a 24- and a 30-column pane, taken for RT-1445: the INSERT footer wraps, its head row ending at `-- INSERT -- · Enter to` (24) or `-- INSERT -- · Enter to select` (30), while the NORMAL footer keeps `Enter to select · ↑/↓ to` on one row. `v2322-vim-bg-shell-{insert,normal}-w120.txt` are an idle box with one background shell in vim INSERT mode (mode line `  -- INSERT -- ⏸ manual mode on · 1 shell · ← 3 agents`) and in NORMAL mode (no head).

`v152-heads-up-collapsed.txt` is the one capture made by hand: `v152-learn-more.txt` with the expanded title and bullet rows taken out and the key row replaced by the collapsed notice's `1: Learn more   2: Knew this already   0: Dismiss`, since no capture of the collapsed notice exists.

`v03-ready-quoting-question-footer--synthesized.txt` is `v03-ready.txt` with a user prompt and a reply quoting a question's footer (`Enter to select · ↑/↓ to navigate · Esc to cancel`) written into the blank rows above the idle box: a quoted footer is transcript, not a dialog.

`v03-ready-quoting-review--synthesized.txt` is the same screen with the reply quoting the review step instead (`Ready to submit your answers?` and its `❯ 1. Submit answers` / `2. Cancel` options), and `v03-ready-draft-under-review-quote--synthesized.txt` is that screen with a draft `1. fix the test` in the box: a quoted review is transcript, not a dialog, and so is a draft under one.

`v42-ask-review-cursor-on-cancel--synthesized.txt` is `v42-ask-review.txt` with the cursor moved from `1. Submit answers` to `2. Cancel`, so the review's matched row sits above the cursor.

`v42-ask-review-w40-bar-wrapped--synthesized.txt` is `v42-ask-review.txt`'s dialog re-wrapped to 40 columns, its tab bar hard-wrapped onto two rows, under a prompt and a reply row; `v42-ask-review-w40-bar-scrolled--synthesized.txt` is the same dialog with its rule, tab bar and title scrolled off the top: both read as the review step without tabs.

The `claude--*` files are the `web/src/fixtures/panes/` panes from collie, https://github.com/leftos/collie at commit 82c6bba7, SGR-coloured text under the MIT licence (see `LICENSE-collie` beside them). The exception is `claude--mcp-server-prompt.txt`, a plain-text capture of Claude Code's new-MCP-server dialog taken in this repo.

`claude--suggestion.txt` and `claude--suggestion-typed.txt` are SGR-coloured captures taken in this repo from Claude Code 2.1.325 under ConPTY at 120x40, with prompt suggestions on and `FORCE_COLOR=3`, on a session's second turn: the idle box showing Claude's suggested next prompt drawn dim (`❯ write todo.py`, SGR 2 alone, as the empty box's `Try "…"` placeholder is drawn), and the same session with `hello` typed, which replaced the suggestion with text not drawn dim. The working folder in their banner is a neutral path of the same length in place of the scratch folder they were taken in.

The `claude--draft-*.txt` fixtures and `claude--suggestion-cut.txt` are SGR-coloured captures from Claude Code 2.1.325 under ConPTY at 120x40, scrubbed the same way, of the prompt box holding a draft: a one-line draft (`draft-one-line`), one long line Ink wrapped onto a second row (`draft-wrapped`), two lines joined by a real newline typed with ctrl+j (`draft-two-lines`), ten such lines (`draft-ten-rows`), a draft taller than the box, which scrolls (`draft-scrolled`), a bracketed paste drawn as its `[Pasted text #1 +N lines]` placeholder with default attributes (`draft-pasted`), and a suggestion cut with `…` (U+2026) in its last cell at a 30-column width (`suggestion-cut`).

`claude--draft-tallest-24.txt`, `claude--draft-scrolled-24.txt`, `claude--draft-tallest-30.txt` and `claude--draft-scrolled-30.txt` are the same kind of capture at 120x24 and 120x30, from Claude Code 2.1.325 run with `--safe-mode`: the tallest draft the box draws whole (`line 1` to `line 7`, and to `line 10`), and a draft about twice that tall, which scrolls (`line 8` to `line 14`, and `line 11` to `line 20`).

What the captures measured about the box, which the draft reader and the clear recipe rely on:
- The box draws at most 7 body rows at 24 screen rows, 10 at 30 and 15 at 40; a taller draft keeps that height and shows its last lines. A scrolled box carries no marker, and `❯` sits on the first row it shows, so a box at that height cannot be told from a scrolled one and the reader treats it as not whole.
- The row above the box's top rule carries passing notices (the effort level, `ctrl+g to edit in Notepad`, a plan usage notice) that come and go with drafts of any height; they say nothing about scrolling.

- A row's line-wrap flag does not tell a wrapped row from a real newline: ConPTY repaints some newline rows by autowrap too, so both can carry it. Ink wraps at a word boundary, so a wrapped row can stop short of the box's inner width.
- ctrl+u (0x15) deletes from the cursor to the start of its line; at the start of a line it deletes the newline before it, so a draft of N lines takes 2N-1 presses. At the start of the first line, and on an empty box, it does nothing, and an empty-box press leaves the kill buffer alone. After a press Claude shows `Ctrl+Y to paste deleted text` above the box.
- Down past the last line is harmless and leaves the cursor at the draft's end; ctrl+e moves to the end of a line, ctrl+k deletes to its end.
- A pasted-text placeholder counts as one line: one ctrl+u clears it.
- ctrl+u bytes written together act one per byte up to a 40-byte write; an 88-byte write was dropped whole, so a recipe writes them in small groups.
- Tab accepts a suggestion as plain text, the whole of it, even when the box shows it cut.

`claude--v2.1.329-model-*.txt` and `claude--v2.1.329-effort-status.txt` are Claude Code 2.1.329 screens (120 columns) taken for RT-1984: the `/model` picker and its session-only `s` paths, the effort row moved, the `Switch model?` dialog and its two answers, `More models`, an Esc close, and the picker opened over a running turn (`docs/spikes/composer-commands.md`, "The picker's session-only paths on 2.1.329").

`claude--v2330-ask-review-w54.txt` and `claude--v2330-ask-review-w120.txt` are Claude Code 2.1.330 screens (54 and 120 columns) taken for RT-2035: the review step of a two-question `AskUserQuestion` with both questions answered, which 2.1.330 draws with no `Ready to submit your answers?` row, its options following the answers directly. The working folder in their banner is a neutral path of the same length in place of the worktree they were taken in.

`claude--v2330-ask-review-gutter-w54--synthesized.txt` is the 54-column capture above with its dialog replaced by one from a transcription of the owner's screen, whose long first question wraps behind a `│` gutter: each of the question's rows carries the gutter, which is stripped so the question reads whole. Its bar's two tab labels and its prompt are invented.

`claude--v2330-review-quoted-with-bar--synthesized.txt` is a reply quoting that 2.1.330 review step whole, its tab bar included, above the idle box: a quoted review is transcript, not a dialog. `claude--v2330-review-under-quoted-ready--synthesized.txt` is a reply quoting a 2.1.307 review's `Ready to submit your answers?` row and options above a live 2.1.330 review whose bar has scrolled off the top: the quoted row is not the live step's own, and the live review reads as the dialog.

`screen/rules.rs` and `screen/dialogs.rs` load each file with `include_str!` as one screen (`screen/fixture_screen.rs`): its lines are the rows, fed to a grid wide enough for the widest of them.

`every_fixture_reads_its_state` pins the state each one must read as, and `every_fixture_reads_its_dialog` the input box and dialog the chat reads off it; a capture whose state changes upstream is replaced with a fresh one, never reinterpreted by editing the rule table or the parsers.
