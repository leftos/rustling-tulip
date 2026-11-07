# Switching the terminal emulation to libghostty

Researched 2026-11-07 for RT-1189, a day after [libghostty 1.0](https://mitchellh.com/writing/libghostty-1-0). The question: should the native client and the daemon move from `alacritty_terminal` 0.26 to libghostty?

## Verdict

Not now. libghostty-vt fits our shape well, but nothing booked is an emulator defect it would fix, its Rust bindings are still pre-1.0, and it adds a Zig toolchain to every build machine. Two steps make a later switch cheap and measured: put our own types behind `term.rs`, then try the daemon's headless screen on libghostty-vt behind a feature once the bindings ship against the 1.0 ABI.

## What libghostty 1.0 is

- 1.0 is **libghostty-vt**: Ghostty's terminal emulation core as a C library, MIT, with a stable semver C ABI checked in CI. It parses bytes and holds terminal state: screen, cursor, scrollback with reflow, the alternate screen, selection, search, a plain-text, VT and HTML formatter, key and mouse encoders, and per-row dirty tracking. It does **no rendering**, so our GPUI renderer stays either way ([headers](https://github.com/ghostty-org/ghostty/tree/main/include/ghostty/vt)).
- The headers cover synchronized output (mode 2026), bracketed paste, OSC 8 hyperlinks, a semantic-prompt OSC command, Kitty graphics, grapheme clusters, and mouse and focus encoders. Kitty keyboard flags and a scrollback limit setting were not confirmed.
- Users include the JetBrains IDE terminal, Neovim, Turborepo, OrbStack and herdr. The only performance figure published is 10,000 live terminals on 4 cores and 8 GB; the throughput claim has no numbers.

## The Rust bindings

- `libghostty-vt` (safe) and `libghostty-vt-sys` (raw FFI) from [libghostty-rs](https://github.com/uzaaft/libghostty-rs), community-maintained. The latest, 0.2.2, predates 1.0 and calls its API unstable.
- The build script fetches Ghostty's source at a pinned commit and builds it with **Zig 0.16 on PATH**, linking statically; environment variables allow an offline or local-source build. Upstream ships prebuilt Windows (GNU ABI, MSVC-linkable) and macOS libraries. Its CI builds `x86_64-pc-windows-msvc`; an upstream MSVC CRT linking fix for the shared library was still open.
- Every handle is `!Send + !Sync`: a terminal lives on one thread. Bytes go in with `vt_write`, replies the terminal writes back come out through an `on_pty_write` callback, and the screen is read through a render state's row and cell iterators.

## What we use today

`alacritty_terminal` 0.26, unpatched, in both `apps/native` and `crates/daemon`:

- The client's `Terminal` in `term.rs` wraps it partly, but its types leak into a dozen other files: `Rgb` is the app-wide colour type, and `CursorShape`, cell `Flags`, `Point`, `Side`, `SelectionType`, `TermMode` and `vte::Perform` appear outside `term.rs`.
- Rendering walks the whole visible grid on every frame (`Terminal::snapshot`); there is no damage tracking. alacritty's `RegexSearch` and `selection_to_string` are not used.
- We parse escapes alacritty does not handle ourselves: OSC 133 and 633 marks (`shell_marks.rs`), mouse modes 1015 and 1016 (`mouse.rs`), synchronized output with a 150 ms timeout, colour query replies and OSC 52 reads.
- The daemon keeps one headless terminal per session in `screen.rs`, fed every PTY chunk and read as text for the status rules, dialogs, the `screen` and `read` verbs, the chat mirror and the login tail. It is the one file to swap on that side.
- The terminal's tests are many: the `ui_terminal` specs, `term.rs`'s unit tests, and the link, mouse, input, shell-mark and copy tests beside them.

## What a switch would buy

- Per-row damage tracking, so a frame repaints only changed rows instead of walking the grid.
- Synchronized output, OSC 133 and Kitty graphics handled by the emulator instead of our own scanners, and grapheme clusters done properly.
- The formatter's plain-text dump for the daemon's screen reads.

## What it would cost

- A Zig 0.16 toolchain on this PC, the builders, the Mac mini and every installer build, or vendored prebuilt libraries and their MSVC linking.
- Pre-1.0 bindings that still have to move to the 1.0 ABI.
- Moving the client's colour, cell and mode types off alacritty first, then the renderer, mouse, input and search code onto new cell iterators, with the whole terminal test suite kept green.
- Single-thread handles: the client's terminal stays on GPUI's main thread as today; each daemon screen needs its own thread or task.

## Next steps (booked as RT-1193 and RT-1194)

1. Own the terminal's types behind `term.rs`: a colour, cursor, cell-flag and mode type of our own, so no `alacritty_terminal` type leaves `term.rs` and `screen.rs`. Worth doing for its own sake, and it turns a later switch into one file per side.
2. Once `libghostty-vt` ships against the 1.0 ABI: the daemon's `screen.rs` on libghostty-vt behind a cargo feature, measuring cold build time with Zig, binary size, and throughput on a large output burst against alacritty, with the `screen.rs` tests green on both. The client switch is decided on those numbers.
