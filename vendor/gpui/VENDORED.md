# Vendored gpui 0.2.2

A trimmed copy of `gpui` 0.2.2 from crates.io (`examples`, `docs` and `tests` left out), used through `[patch.crates-io]` in the workspace `Cargo.toml`, for two fixes.

**The first fix.** `TextLayout::layout`'s measure closure in `src/elements/text.rs` shortened its one `runs` list when it truncated a line, then shaped the whole text with the short list on the next, wider measure, which panics when the stale length lands inside a character. Each measure now truncates its own copy.

**The second fix.** The press that brings a window forward is marked as `MouseDownEvent::first_mouse`, whatever its button, so a client can treat it as focus-only. On Windows, a `WM_MOUSEACTIVATE` whose hit test is `HTCLIENT` marks the next mouse-down as that press. On macOS, the first-mouse flag now marks and clears on the next press of any button, including a Ctrl-left click mapped to a right one.

Remove this folder and the patch entry when the client moves to a gpui revision that carries both fixes.
