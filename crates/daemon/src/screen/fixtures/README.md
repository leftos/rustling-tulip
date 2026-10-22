# Screen fixtures

The spike captures (`v03-ready.txt`, `c11-turn1-end.txt`, `p98-compacted.txt`, `v151-*` and the rest of the `v*`/`c*`/`p*` names) are Claude Code 2.1.307 screens captured for `docs/spikes/chat-mirror-tui.md`, plain text with no escape sequences.

The `claude--*` files are the `web/src/fixtures/panes/` panes from collie, https://github.com/leftos/collie at commit 82c6bba7, SGR-coloured text under the MIT licence (see `LICENSE-collie` beside them).

`screen/rules.rs` loads each file with `include_str!` as one screen: its lines are the rows, fed to a grid wide enough for the widest of them.

`every_fixture_reads_its_state` pins the state each one must read as; a capture whose state changes upstream is replaced with a fresh one, never reinterpreted by editing the rule table.
