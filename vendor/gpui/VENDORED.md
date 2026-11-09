# Vendored gpui 0.2.2

A trimmed copy of `gpui` 0.2.2 from crates.io (`examples`, `docs` and `tests` left out), used through `[patch.crates-io]` in the workspace `Cargo.toml`, for one fix.

**The fix.** `TextLayout::layout`'s measure closure in `src/elements/text.rs` shortened its one `runs` list when it truncated a line, then shaped the whole text with the short list on the next, wider measure, which panics when the stale length lands inside a character. Each measure now truncates its own copy.

Remove this folder and the patch entry when the client moves to a gpui revision that carries the fix.
