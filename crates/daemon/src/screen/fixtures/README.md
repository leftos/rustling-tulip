# Screen fixtures

The spike captures (`v03-ready.txt`, `c11-turn1-end.txt`, `p98-compacted.txt` and the rest of the `v*`/`c*`/`p*` names) are Claude Code 2.1.307 screens captured for `docs/spikes/chat-mirror-tui.md`, plain text with no escape sequences.

`v160-…` to `v172-…` are Claude Code 2.1.312 screens (152 columns) captured for the "The agent in view (CV.23)" section of `docs/spikes/chat-mirror-tui.md`: the main view with the agents panel, a subagent's transcript in view, and drafts and prompts across a switch. `v169`, `v170` and `v171` show a Heads up look-alike drawn by a spike mod, not the built-in notice: its band and keys differ in look, while its draft lands through the same `prompt.fill` call.

`v152-heads-up-collapsed.txt` is the one capture made by hand: `v152-learn-more.txt` with the expanded title and bullet rows taken out and the key row replaced by the collapsed notice's `1: Learn more   2: Knew this already   0: Dismiss`, since no capture of the collapsed notice exists.

The `claude--*` files are the `web/src/fixtures/panes/` panes from collie, https://github.com/leftos/collie at commit 82c6bba7, SGR-coloured text under the MIT licence (see `LICENSE-collie` beside them).

`screen/rules.rs` and `screen/dialogs.rs` load each file with `include_str!` as one screen (`screen/fixture_screen.rs`): its lines are the rows, fed to a grid wide enough for the widest of them.

`every_fixture_reads_its_state` pins the state each one must read as, and `every_fixture_reads_its_dialog` the input box and dialog the chat reads off it; a capture whose state changes upstream is replaced with a fresh one, never reinterpreted by editing the rule table or the parsers.
