# RT-169 terminal rendering audit: native client vs xterm (Tauri `tauri-last`)

Scope read from `docs/plans/native-client-parity.md` (Terminal item "**(hard)** Rendering parity with xterm 6 + WebGL (Fit, WebFonts, Clipboard addons; DOM fallback on GPU context loss)") and `docs/native-client.md` (## Terminal).

Method: static comparison of the native client at `X:/dev/rustling-tulip.wt/rt169-audit` (branch `rt169-audit`, cut from `main` c0b3bdd) against the retired Tauri client at tag `tauri-last` (commit 62e8990), its `apps/tauri-app/package.json` (`@xterm/xterm` 6.1.0-beta.219, addon-fit 0.12.0-beta.219, addon-web-fonts 0.2.0-beta.133, addon-webgl 0.20.0-beta.218, addon-clipboard 0.3.0-beta.219), and the exact xterm/addon package sources unpacked from npm into `.tmp/xt/`. Native dependency sources checked: `alacritty_terminal` 0.26.0, `vte` 0.15.0, `gpui` 0.2.2.

Runtime evidence: `gate run ... -- pwsh ./rt.ps1 native-shot main` built the client and wrote `.tmp/shots/main.png` (984x601) over the fake daemon; the shot process then exited 1 on `timed out waiting on app_will_quit` after writing its last capture (pre-existing shot-example quit flake, not a rendering failure). The capture was inspected: panes, tabs and terminal text render correctly. Pixel band measurement of the lower terminal pane gives row pitch 17 px at the default font (Geist Mono 13 px), which matches `ceil(ascent + descent) = ceil(16.9)` in `fonts::line_height`. No live daemon or real PTY was started; no tracked file was edited.

Verdict scale: **matches** / **acceptable difference** / **fix needed**. Anything not actually exercised at runtime says so.

Summary: 4 fix-needed rendering differences (ligatures, dropped zero-width glyphs, unrendered SGR text attributes, OSC 8), 1 fix-needed clipboard difference (image-only Ctrl+V), plus smaller acceptable differences and one fix-needed low-priority mouse encoding gap. The two owner checks: the image-only Ctrl+V check fails; the unchanged-position mouse-move check passes by inspection.

---

## 1. Fit (cell size and cols/rows for a pane; refit on resize, font change, scrollback drawn)

### F1 — Scrollbar width is in xterm's fit but not native's, so column counts differ
- **Repro**: open any session with more than one screen of output in a pane 800 px wide, default Geist Mono 13 px (cell 7.8 px).
- **Native**: no scrollbar is drawn anywhere (`rg -i scrollbar apps/native/src` has no hit); `grid_size` is `floor(bounds.width / cell_width)` (`apps/native/src/term_view.rs:1674-1689`), so 102 columns.
- **xterm/Tauri**: xterm 6 shows a scrollbar and `FitAddon.proposeDimensions` subtracts `scrollbar.width ?? 14` (unless `scrollback === 0` or `showScrollbar` false) and the terminal's padding: `max(2, floor((width - 14) / cellWidth))`, so 100 columns. Tauri sets `scrollback: 5000` and never disables the scrollbar, so 14 px is always subtracted (`Terminal.tsx:363-364`; addon-fit source).
- **Verdict**: acceptable difference if the scrollbar-less design is intentional (the grid is 1-2 columns wider and wheel scrolling still works); otherwise fix needed for literal fit parity. Also native's floor minimum is 1 column, xterm's is 2 (`apps/native/src/term_view.rs:1682`).

### F2 — Cell height is rounded up before the row count; xterm divides by the fractional height
- **Repro**: set the terminal area to 508 px tall, Geist Mono 13 px.
- **Native**: `line_height` is `ceil(ascent + |descent|)` = `ceil(16.9)` = 17 px (`apps/native/src/fonts.rs:198-205`, `term_view.rs:1664-1671`), so `floor(508 / 17)` = 29 rows. The 17 px pitch is measured in `.tmp/shots/main.png` (row bands start at 399, 416, 434, 451, 468 px).
- **xterm/Tauri**: the canvas measure strategy uses `fontBoundingBoxAscent + fontBoundingBoxDescent` un-rounded (16.9 for Geist Mono per the font's hhea 1005/-295 at upem 1000) and `FitAddon` divides by it, so `floor(508 / 16.9)` = 30 rows. (Derived from xterm/addon sources and the font tables; not run against WebView2.)
- **Verdict**: acceptable difference; a one-row boundary case, and native's rows land on whole pixels (crisper text).

### F3 — Refit triggers: resize, font change, scrollback drawn
- **Repro**: drag the window smaller; change font family or size in Appearance; reopen a pane with 5000 lines of history.
- **Native**: the canvas prepaint recomputes metrics and calls `ensure_size(grid_size(bounds, &m))` on every layout (`term_view.rs:1482-1492`), and `set_font` clears the resolved family and notifies (`term_view.rs:308-316`), so a resize, a font change and a new pane size all refit before the next frame. Scrollback replay does not change bounds or metrics, so it needs no separate fit.
- **xterm/Tauri**: `ResizeObserver` → `fit.fit()` + resize (`Terminal.tsx:932-941`), font effects → `fit.fit()` + resize (`Terminal.tsx:258-304`), and a `requestAnimationFrame` fit after scrollback is drawn (`Terminal.tsx:786-792`).
- **Verdict**: matches (native's per-paint refit subsumes the post-scrollback fit).

### F4 — Only one pane per session drives PTY size
- **Repro**: show one session in two panes of different sizes.
- **Native**: `SizeGate` lets only the pane marked the session's size driver send `Resize` (`term_view.rs:153-181`, `875-905`).
- **xterm/Tauri**: every pane's own xterm ResizeObserver sends its own `resize`, so two panes on one session overwrite each other's PTY size.
- **Verdict**: acceptable difference (native is the better behaviour; matches the documented one-size-driver rule).

---

## 2. Fonts (glyph fallback, bold/italic, ligatures, line height, bundled vs system)

### N1 — Ligatures are on for three of the four bundled families, and native's per-glyph width forcing misplaces text after a ligature
- **Repro**: Settings → Appearance → font family `Fira Code` (or JetBrains Mono or Cascadia Code), then `printf 'a -> b\n'` in a shell pane. Also `=>`, `!=`, `...`, `<=`, `|>`.
- **Native**: `metrics()` builds a plain `gpui::Font` (`term_view.rs:1649-1657`) with `FontFeatures::default()`, and GPUI's DirectWrite path leaves DirectWrite's default `liga`/`clig`/`calt` enabled (`gpui-0.2.2/src/platform/windows/direct_write.rs:1725-1765`, whose comment states these are enabled by default). Verified with the bundled fonts' GSUB tables: Cascadia Code, Fira Code and JetBrains Mono all carry `calt`; Geist Mono does not (default is unaffected). `paint_span` shapes a whole same-style span and then GPUI's `force_width` loop repositions glyph **index** i at `i * cell_width` (`gpui-0.2.2/src/text_system/line_layout.rs:568-575`), not character i. A `->` ligature is one glyph for two characters, so every later glyph in that span is drawn one cell to the left of its true column.
- **xterm/Tauri**: xterm has no ligature shaping — its canvas/WebGL renderers draw one glyph per cell and no ligature addon is loaded (`package.json` has no addon-ligatures), so `->` is two separate glyphs and nothing shifts.
- **Verdict**: **fix needed**. Disable ligatures for terminal shaping (e.g. `FontFeatures` with `liga`/`clig`/`calt` = 0) or shape per cell; the misalignment is inferred statically from the code paths, not captured in a screenshot.

### N2 — Zero-width and combining characters are dropped
- **Repro**: `printf 'e\xcc\x81\n'` (decomposed é) and `printf '\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7\n'` (family emoji, ZWJ + VS16).
- **Native**: `push_cell` reads only `cell.c` (`apps/native/src/term.rs:1092-1105`); alacritty stores combining marks, variation selectors and ZWJ in `cell.zerowidth()`/`cell.extra`, and nothing in the native snapshot or painter reads that. All four bundled mono fonts do contain U+0301 (verified via cmap), so the accent is available but never drawn; the composed emoji is drawn as its base components. `grid_text()` shows the same loss to tests.
- **xterm/Tauri**: xterm joins zero-width characters into the preceding cell (`extractShouldJoin` / `_activeBuffer` handling in xterm 6) and the renderer draws the combined string, so é and the family emoji render composed.
- **Verdict**: **fix needed** (correctness of displayed text, not just cosmetics).

### N3 — Glyph fallback: DirectWrite system fallback instead of xterm's CSS family cascade
- **Repro**: pick a system family that lacks box drawing or Powerline glyphs, then print `┌─┬┐` and Powerline `\ue0b0`.
- **Native**: `resolve_family` picks exactly one family, or the first of Geist Mono / Cascadia Mono / Consolas / Courier New when the requested family is not installed (`apps/native/src/fonts.rs:25-26,174-192`), and the `Font` carries no fallback list, so a missing glyph goes to DirectWrite's Unicode-range system fallback. Box drawing is present in all four bundled families (cmap-checked); Powerline PUA is present only in Fira Code and JetBrains Mono, so the default Geist Mono and Cascadia Code depend on a system Powerline font in both clients.
- **xterm/Tauri**: `fontFamily` is `'picked', 'Geist Mono', 'Cascadia Mono', Consolas, 'Courier New', monospace` (`Terminal.tsx:42-53`), so after the picked family the browser tries Cascadia Mono/Consolas specifically before the system fallback.
- **Verdict**: acceptable difference. Both fall back for CJK/emoji; the intermediate cascade can only matter for glyphs Cascadia/Consolas have and the picked family lacks, and box drawing is covered by every bundled family. Exact fallback pixels are unmeasured.

### N4 — Wide characters keep their natural advance inside a 2-cell footprint
- **Repro**: `printf '\xe4\xb8\x80\xf0\x9f\x98\x80\n'` (CJK 一, emoji) in default Geist Mono 13 px.
- **Native**: alacritty marks these `WIDE_CHAR` (width 2); `push_cell` builds a one-glyph span and `paint_span` deliberately skips `force_width` for it (`term_view.rs:1910`). The glyph is drawn at its own natural advance (CJK fallback is typically 1 em = 13 px) inside a 15.6 px (2 x 7.8) footprint, so the glyph can look narrow; the next span still starts at its true column because each span is positioned by `col`.
- **xterm/Tauri**: each cell is drawn by column; wide glyphs are placed in a 2-cell box. xterm's default width tables are Unicode 6 (no `addon-unicode11` in the Tauri app), while alacritty uses current `unicode-width`, so ambiguous/newer emoji can also be classified wide/narrow differently.
- **Verdict**: acceptable difference in alignment; exact glyph widths and ambiguous-width cases are **unmeasured**.

### N5 — Bold/italic, line height and bundled families
- **Repro**: `printf '\e[1mbold\e[22m \e[3mitalic\e[23m\n'`, toggle Bold in Appearance, switch among the bundled families.
- **Native**: bold maps to `FontWeight::BOLD`, bold base ANSI colours are brightened (`term.rs:1076,1143-1153`), italic maps to `FontStyle::Italic` (`term_view.rs:1830-1858`). Geist Mono, JetBrains Mono and Cascadia Code bundle Regular/Bold/Italic/BoldItalic; Fira Code bundles Regular/Bold only (`fonts.rs:49-92`). Line height is `ceil(ascent+|descent|)` and 17 px at 13 px measured in the shot.
- **xterm/Tauri**: `fontWeight` normal/bold, `fontWeightBold` defaults to bold, italic is a real italic face where present and synthetic oblique where not (Fira Code), line height option left at 1.
- **Verdict**: matches. The native theme's colours intentionally differ from Tauri's (`theme.rs:4-6`) but were outside these axes.

---

## 3. Clipboard

### C1 — Plain Ctrl+V with an image-only clipboard sends nothing instead of ^V (owner check)
- **Repro**: put an image on the Windows clipboard (Win+Shift+S), focus a shell pane, press Ctrl+V.
- **Native**: `key_action` maps Ctrl+V/Ctrl+Shift+V to `KeyAction::Paste` unconditionally (`apps/native/src/term_input.rs:80-90`), then `paste()` does `cx.read_from_clipboard().and_then(|item| item.text())` and returns when it is `None` (`term_view.rs:972-975`). GPUI's Windows `ClipboardItem::text()` returns `None` when the item has only image entries (`gpui-0.2.2/src/platform.rs:1553-1565`; the Windows reader maps PNG/JFIF/GIF/SVG formats to images). So no text, no bytes, and the keystroke is consumed — the agent never sees ^V.
- **xterm/Tauri**: Ctrl+V is deliberately passed through to the browser paste event (`Terminal.tsx:944-946`, `term.attachCustomKeyEventHandler` → return false), and with an image-only clipboard the paste handler finds `chosen.length === 0` and injects nothing. Tauri therefore also sends nothing, but the owner's stated acceptance is that native falls back to ^V.
- **Verdict**: **fix needed**. On a clipboard with no text, send `0x16` (^V) to the child instead of consuming the key.

### C2 — Ctrl+C clears the selection after copying
- **Repro**: drag-select terminal text, press Ctrl+C, then look at the selection.
- **Native**: `KeyAction::Copy { clear_selection: !m.shift }` (`term_input.rs:83-87`) clears the highlight after the copy.
- **xterm/Tauri**: copies via `copyToClipboard` and leaves the selection in place.
- **Verdict**: acceptable difference (minor UX), unless selection persistence is wanted.

### C3 — Copy-on-select fires on mouse-up, not on every selection change
- **Repro**: drag-select slowly with copy-on-select on.
- **Native**: copies once when the button comes up (`term_view.rs:1266-1288`).
- **xterm/Tauri**: `onSelectionChange` copies on every selection tick during the drag (`Terminal.tsx:845-860`); the last write wins.
- **Verdict**: acceptable difference (same end state, fewer clipboard writes).

### C4 — OSC 52
- **Repro**: `printf '\e]52;c;%s\a' "$(printf 'hello' | base64)"`, and `printf '\e]52;c;?\a'`.
- **Native**: `Osc52::CopyPaste` (`term.rs:466-469`), stores collected and emitted as `PaneEvent::Copied` → `copy_to_clipboard` (GPUI system clipboard + chip) (`term_view.rs:780-790`, `copied.rs:99-104`); reads are answered with an empty clipboard (`term.rs:37-45`, test `osc52_load_is_answered_with_an_empty_clipboard`); stores replayed from scrollback history are discarded (test `osc52_store_in_history_is_discarded`); only the pane that answers queries processes them, so two panes on one session copy once.
- **xterm/Tauri**: ClipboardAddon + `RtClipboardProvider` write via `navigator.clipboard.writeText` and answer reads with `""`; each pane's own xterm would process a replay, and two panes would each write.
- **Verdict**: matches for the security-relevant behaviour; the history-store drop and single-pane answer are acceptable (better) differences.

### C5 — Paste sanitisation
- **Repro**: copy text containing a literal `\x1b[201~`, then paste into a bracketed-paste application.
- **Native**: strips embedded bracket markers so the paste cannot end early (`term_input.rs:125-149`).
- **xterm/Tauri**: `term.paste()` wraps the raw text in `\x1b[200~`/`\x1b[201~` and does not strip embedded markers.
- **Verdict**: acceptable difference (safer).

---

## 4. GPU context loss

### G1 — Native auto-recreates the device; xterm falls back to a DOM renderer, and native has no software fallback
- **Repro**: force a TDR/device removal (e.g. `dxcap`, a driver reset, or a GPU hang) while the client is open.
- **Native**: GPUI's Windows platform checks `ID3D11Device::GetDeviceRemovedReason()` each vsync; on loss it waits 350 ms, recreates `DirectXDevices` and the renderer resources (swap chain, pipelines, glyph atlas) with up to 5 attempts 100 ms apart, clears the text/glyph caches, and sends `WM_GPUI_GPU_DEVICE_LOST` so the window redraws (`gpui-0.2.2/src/platform/windows/platform.rs:252-253,1022-1070`, `directx_devices.rs:23-42`, `directx_renderer.rs:209-285`). The terminal grid lives in `alacritty_terminal` on the CPU, so no content is lost. If all 5 attempts fail, GPUI logs `Failed to recover DirectX devices after multiple attempts` and the commented-out `std::process::exit(1)` means the app keeps running with an unrecoverable renderer — there is no DOM or software renderer to fall back to.
- **xterm/Tauri**: `webgl.onContextLoss(() => webgl.dispose())` (`Terminal.tsx:466-470`); disposing the WebGL addon lets xterm activate its DOM renderer, which keeps rendering the same buffer.
- **Verdict**: acceptable difference for the normal TDR path (automatic device recreation is the native equivalent of the WebGL→DOM fallback); the permanent-failure path has no fallback and is a residual gap. Not simulated at runtime.

---

## 5. Escape sequences where alacritty_terminal and xterm differ

### E1 — Extended underline styles are parsed but never drawn
- **Repro**: `printf '\e[4:1mS\e[4:2mD\e[4:3mC\e[4:4mD\e[4:5mS\e[0m\n'`.
- **Native**: `alacritty_terminal` 0.26 parses `4:2`..`4:5` into `Flags::DOUBLE_UNDERLINE`, `UNDERCURL`, `DOTTED_UNDERLINE`, `DASHED_UNDERLINE` (`term/mod.rs:1903-1922`), but `SPAN_STYLE` only includes `UNDERLINE` (`apps/native/src/term.rs:393-396`) and `text_run` draws only a single 1 px line for `Flags::UNDERLINE` (`term_view.rs:1849-1856`). The other flags are dropped from the span mask entirely, so a run using only e.g. `4:2` is merged with neighbouring unstyled text and shows no underline at all.
- **xterm/Tauri**: xterm 6 parses `4:N` for N 1-5 into `extended.underlineStyle` and its renderers draw single/double/curly/dotted/dashed (`xterm.mjs` `_processUnderline`, DOM classes `xterm-underline-N`).
- **Verdict**: **fix needed**.

### E2 — Strikethrough (SGR 9) and dim (SGR 2) are parsed but not rendered
- **Repro**: `printf '\e[9mstruck\e[0m \e[2mdim\e[0m\n'`.
- **Native**: alacritty sets `Flags::STRIKEOUT` / `Flags::DIM` (`term/mod.rs:1898,1925`), but `SPAN_STYLE` omits both and `TextRun.strikethrough` is always `None` (`term_view.rs:1857`); `push_cell` never dims the foreground. Both render as plain text.
- **xterm/Tauri**: xterm exposes `isStrikethrough()` / `isDim()` and the DOM/WebGL renderers apply `xterm-strikethrough` and dimming.
- **Verdict**: **fix needed** (strikethrough changes meaning in diffs/todos; dim is cosmetic). Overline (SGR 53) is additionally supported by xterm 6 but not parsed at all by alacritty 0.26.

### E3 — Underline colour (SGR 58) is ignored
- **Repro**: `printf '\e[4m\e[58:2::ff0000mred underline\e[0m\n'`.
- **Native**: alacritty stores the underline colour (`Attr::UnderlineColor`), but native never reads the cell's underline colour and uses the foreground for the underline (`term_view.rs:1849-1856`).
- **xterm/Tauri**: xterm parses `58` and sets `textDecorationColor` per cell.
- **Verdict**: **fix needed** (low impact).

### E4 — OSC 8 hyperlinks are stored but never exposed or activated
- **Repro**: `printf '\e]8;;https://example.com\e\\click\e]8;;\e\\\n'`, then Ctrl+click `click`.
- **Native**: alacritty stores the hyperlink on each cell (`cell.hyperlink()` in `alacritty_terminal`), but the snapshot (`term.rs:826-850`) and link layer (`links.rs`) never read it. Only visible URL text is detected, so a link whose text is not a URL is dead; an OSC 8 URL whose text is also a URL happens to work through the text detector.
- **xterm/Tauri**: xterm 6 has a built-in `OscLinkService` provider that reports each OSC 8 run to link handlers; with no `linkHandler` set it uses its default activation (confirm, then open http/https). Tauri does not disable it.
- **Verdict**: **fix needed** for parity (OSC 8 is used by Claude's own TUI and by modern shells/editors).

### E5 — Synchronized output (DEC 2026)
- **Repro**: a program sends `\e[?2026h`, streams for 500 ms, then `\e[?2026l`.
- **Native**: supported via `alacritty_terminal`/`vte`; `SYNC_TIMEOUT` is 150 ms (`term.rs:78`, matching `vte`'s `SYNC_UPDATE_TIMEOUT`), and the pane ends an overlong update itself (`term_view.rs:794-842`).
- **xterm/Tauri**: xterm 6 also supports DEC 2026 but its `SyncOutputHandler` flushes at 1000 ms.
- **Verdict**: acceptable difference (native may paint a partial frame where xterm holds it longer; programs are expected to close the update promptly). Native otherwise supports the feature, which older xterm.js did not.

### E6 — Cursor shapes and blink
- **Repro**: `printf '\e[2 q\e[4 q\e[6 q\e[?25l\e[?25h'` and a program that requests a blinking shape (`\e[1 q`).
- **Native**: alacritty maps DECSCUSR 1|2 → block, 3|4 → underline, 5|6 → beam and discards the blink flag (`vte-0.15.0/src/ansi.rs:1716-1718`); `paint_cursor` draws block/beam/underline/hollow and hides on `Hidden` (`term_view.rs:1804-1828`), and `default_cursor_style` gives agents a beam and shells a block (`term_input.rs:33-41`).
- **xterm/Tauri**: same shape mapping; xterm keeps a per-mode `cursorBlink` from DECSCUSR and would blink where the program asked even though `cursorBlink: false` was set on options (`xterm.mjs` renderer reads `decPrivateModes.cursorBlink ?? options.cursorBlink`).
- **Verdict**: acceptable difference (no blink is the documented design, `docs/native-client.md` "Cursor: bar for agents, block for shells, no blink").

### E7 — Mouse-report encodings (adjacent to the rendering axis)
- **Repro**: `printf '\e[?1016h'` (SGR-pixel) or `\e[?1015h` (urxvt) then move/click.
- **Native**: only X10, UTF-8 (1005) and SGR (1006) are encoded (`apps/native/src/mouse.rs:98-155`).
- **xterm/Tauri**: xterm 6 also decodes 1015 and 1016 (SGR-pixel).
- **Verdict**: acceptable difference (rarely requested; SGR 1006 is what TUIs use). If pixel-precise apps matter, this is a fix.

---

## 6. The owner's two checks

### O1 — Ctrl+V with an image-only clipboard must reach the agent as ^V
- See C1. **Fix needed**: `paste()` returns without sending anything when `ClipboardItem::text()` is `None`, and `on_key` has already stopped propagation, so the child gets neither a paste nor `0x16`.

### O2 — A Windows mouse-move event whose position is unchanged must not produce hover or motion reports
- **Repro**: with a program requesting any-motion reporting (`\e[?1003h`), post the same mouse position twice; with a link under the cursor and Ctrl held, post the same position twice.
- **Native**: motion reports are deduplicated by cell — `report_motion` returns when `last_motion == Some((row, col))` (`term_view.rs:1255-1262`), for both the no-button path (`on_mouse_move`, `1119-1131`) and the drag path (`on_gesture_move`, `1135-1153`). Hover state is `(ctrl held, cell)` and only notifies when that tuple changes (`on_hover_move`, `1157-1170`); same position therefore produces no report and no repaint. Note this deduplicates by cell, so a sub-cell pixel move is also suppressed (stricter than "same position").
- **xterm/Tauri**: xterm's mouse service computes a cell coordinate and its own dedupe/encoding path; the requirement is native-side.
- **Verdict**: **matches** by code inspection. Not exercised at runtime: no UI spec posts two identical mouse-move events, and doing so would have required adding a test file, which this audit was not allowed to do.

---

## Unmeasured / not compared
- Exact cols/rows in the Tauri app for a given pane size (the Tauri client is not runnable on `main`; the FitAddon formula and xterm sources were used instead).
- Pixel-level glyph fallback for CJK, emoji, Powerline and non-bundled system families (cmap coverage and the shaping paths were checked; no render comparison).
- Wide-glyph advance versus the 2-cell footprint (mechanism identified; no pixel measurement).
- Ligature misalignment (inferred from GPUI's per-glyph width forcing plus the fonts' `calt` tables; no screenshot with a ligature font).
- GPU device-loss recovery (code path read; not simulated).
- OSC 8 activation end-to-end (xterm source read; not clicked).
- The terminal colour palette and selection colours, which are outside these axes; `theme.rs` states intentionally different values from Tauri.
