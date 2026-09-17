# Terminal path opening: OS default handler + wrap stitching

Ctrl-click on a path in a terminal currently always shells out to VS Code
(`open_path_in_vscode` → `spawn_vscode`), and only ever sees one xterm buffer
row, so a path the terminal or a TUI broke across two rows is either not
detected at all or detected as a truncated fragment that fails to resolve.

Two changes:

1. **The OS picks the app.** A plain path goes to the system default handler.
   A path carrying a `:line[:col]` ref still goes to VS Code, because no OS
   handler can honor a line ref — the suffix *is* the request for an editor.
2. **Stitch paths split across rows** before resolving them.

## Decisions (user-confirmed 2026-09-17)

- **Opener** — OS default for plain paths (`notes.pdf` → the PDF app, `docs/`
  → the file manager, `app.ts` → whatever `.ts` is registered to); VS Code
  `-g` for `app.ts:42:7`.
- **Unassociated extension** (`.rs`, `Makefile`, `LICENSE`) — let the OS show
  its own "How do you want to open this file?" picker. No silent VS Code
  fallback; the user's choice there sticks for next time.
- **Wrap merging** — xterm soft wraps merge unconditionally (the buffer flags
  them, there is no guesswork). Hard wraps — a real newline a TUI emitted,
  possibly inside a box — merge structurally, but the merged path only opens
  if it exists on disk; otherwise the click falls back to the un-stitched
  fragment.

## Tasks

- [x] `terminalLinks.ts` — row-aware detection, stitching, open candidates
- [x] `terminalLinks.test.ts` — vitest coverage for detection + stitching
- [x] `Terminal.tsx` — build the row window, emit multi-row `ILink` ranges
- [x] `lib.rs` — `open_terminal_path` replaces `open_path_in_vscode`
- [x] `Sidebar.tsx` — repoint its two `open_path_in_vscode` calls
- [x] `terminal-links.spec.ts` — e2e case for a wrapped path
- [x] Commit, push to `main`, build the installer

## Frontend detection — `apps/tauri-app/src/utils/terminalLinks.ts`

### Types

```ts
export interface TerminalRow {
  /** Row text padded to `cols` — xterm `translateToString(false)`. */
  text: string;
  /** xterm `IBufferLine.isWrapped`: this row continues the one above. */
  isWrapped: boolean;
}

export interface TerminalLinkCandidate {
  path: string;
  line: number | null;
  column: number | null;
}

export interface DetectedTerminalLink {
  kind: TerminalLinkKind;
  /** Full link text, stitching included. */
  text: string;
  /** `candidates[0].path` — the longest reading. */
  target: string;
  line: number | null;
  column: number | null;
  /**
   * Open candidates, longest first. Length 1 unless the link crosses a hard
   * wrap; each extra entry truncates at one stitch boundary, so an over-eager
   * stitch degrades to the fragment that was actually on screen.
   */
  candidates: TerminalLinkCandidate[];
  /** Offsets into the `rows` array; columns are 0-based, `endColumn` exclusive. */
  startRow: number;
  startColumn: number;
  endRow: number;
  endColumn: number;
}
```

`DetectedTerminalLink` also keeps the existing `startIndex` / `endIndex`
offsets into the logical line: `detectTerminalLinks` stays the matching engine
and reports in those terms, and the candidate split below is expressed in
them. For a single-row link `startRow === endRow === 0` and the columns equal
the indices.

`detectTerminalLinks(text: string): DetectedTerminalLink[]` stays exported and
single-line — it remains the matching engine and the unit-test entry point.
New exports:

```ts
export function detectTerminalRowLinks(
  rows: TerminalRow[],
  cols: number,
): DetectedTerminalLink[];

/** Exported because `Terminal.tsx`'s row-window walk applies the same test. */
export function canStitch(
  above: TerminalRow,
  below: TerminalRow,
  cols: number,
): boolean;
```

### Assembling a logical line

Walk `rows` in order, grouping into logical lines. A row joins the group above
it when either:

- **soft wrap** — `row.isWrapped === true`. The whole untrimmed row text is
  appended with no separator and `columnOffset = 0`. Trailing pad spaces are
  harmless: no path pattern matches them, and keeping them makes index → column
  arithmetic exact.
- **hard stitch** — `canStitch(above, row, cols)` (below). The row's
  *frame-stripped* content is appended with no separator, `columnOffset` = the
  column its first content character sits at.

Otherwise the row starts a new logical line. Track each appended piece as a
segment `{ rowIndex, columnOffset, text, stitched }` so a logical-line index
maps back to `(row, column)`, and so the stitch boundaries are known.

At a hard stitch the segment *above* is also cut back to its own `stripFrame`
end (`truncateToContent`). `canStitch` pairs the last character of the row
above with the first character of the row below, so the two must end up
adjacent in the logical line — without the cut, the box's right border and the
pad spaces would land between the two halves of the path and no boxed path
could ever stitch.

Cap a group at `STITCH_LIMIT = 4` hard stitches. Soft wraps are uncapped —
that is one logical line as far as the buffer is concerned.

### Frame stripping

```ts
const FRAME_CHAR = /[\s│┃║╎╏┆┇┊┋▌▐|]/;
const BORDER_CHAR = /[│┃║╎╏┆┇┊┋▌▐|]/;
```

`stripFrame(text)` returns `{ content, start, end }` with `start`/`end` as
0-based columns into the original row (`end` exclusive).

The **trailing** run of frame characters always goes: it is a box's right
border plus the pad spaces xterm reports for the unwritten tail of the row,
and neither is content.

The **leading** run only goes when it contains a `BORDER_CHAR` — that makes it
a box's left edge and its padding. A bare run of spaces is indentation that
belongs to the content, and keeping it is the whole reason an indented row can
be rejected as a continuation: strip it and `b.content[0]` is always a path
character, so nothing in `canStitch` would ever catch the case. This
asymmetry is load-bearing, and the test
"rejects a continuation that starts with whitespace" pins it.

### `canStitch(above, below, cols)`

All of the following, or no stitch:

1. `below.isWrapped === false` — soft wrap is handled by the other branch.
2. `a = stripFrame(above.text)` and `b = stripFrame(below.text)` both have
   non-empty content.
3. `a.content`'s last character and `b.content`'s first character both match
   `PATH_CONTINUATION = /[A-Za-z0-9_.\-\\/~$%+@#]/` — a path run continues
   across the break with no separator.
4. **Flush against the wrap column.** Let `wrapColumn` be the column of the
   trailing non-space frame glyph on `above.text` if it has one (a box's right
   border), else `cols`. Require `a.end >= wrapColumn - STITCH_TOLERANCE`
   with `STITCH_TOLERANCE = 4`. A TUI only wraps a row because it ran out of
   width, so a row that stops well short of its wrap column did not wrap —
   it ended. This is the test that keeps `Wrote to output.txt` / `done` from
   being glued into `output.txtdone`.
5. **Continuation is no longer than the row it continues.** `b`'s content
   length `<=` `a`'s content length. A full row wraps; a row longer than the
   one above it is not a continuation of it.

### Candidates

For each detected link, find the hard-stitch segment boundaries lying strictly
inside `(startIndex, endIndex)`. `candidates[0]` is the full stitched text;
each further entry truncates the text at the next boundary, working from the
last boundary back to the first. Every candidate runs through the existing
`splitPathPosition` so it carries its own `line`/`column` — `src/foo.` has no
line ref even when the merged `src/foo.ts:4` does.

URLs use the same stitching and get a single candidate: a browser cannot
verify a URL the way the filesystem verifies a path, so there is nothing to
fall back to.

## Frontend wiring — `apps/tauri-app/src/components/Terminal.tsx`

In `provideLinks(bufferLineNumber, callback)`:

1. From `row = bufferLineNumber - 1`, walk **back** while the current row is
   `isWrapped` or `canStitch(previous, current, term.cols)`, and **forward**
   on the same rule. Bound the walk at 64 rows so a pathological buffer cannot
   stall a hover.
2. Collect that window as `TerminalRow[]` — `translateToString(false)` plus
   `isWrapped` — and run `detectTerminalRowLinks(rows, term.cols)`.
3. Keep only links whose row span covers the hovered row.
4. Emit ranges in absolute 1-based buffer coordinates, spanning rows where the
   link does:
   `start: { x: startColumn + 1, y: firstRow + startRow + 1 }`,
   `end: { x: endColumn + 1, y: firstRow + endRow + 1 }`.
   This keeps the existing exclusive-end convention, so a stitched link
   underlines across both rows and reads as the one link it is.

`activate` dispatches the existing cancelable `rt:terminal-link-open` event —
detail gains `candidates`, and keeps `target`/`line`/`column` as
`candidates[0]` so existing listeners and the e2e spec keep working — then
invokes `open_terminal_path` for paths, `open_url` for URLs unchanged.

## Backend — `apps/tauri-app/src-tauri/src/lib.rs`

`open_path_in_vscode` has two other callers, both in `Sidebar.tsx`'s
`openContainerInVscode`: a repo root, and a workspace's linked
`.code-workspace` file. Both are context-menu entries labelled "Open in VS
Code", so they must keep opening VS Code — the "no OS handler can honor a line
ref" reasoning is about terminal links and does not apply to a menu item that
names the editor. Repoint both at the existing `open_folders_in_vscode`, which
the same function's third branch already uses: its signature already fits
(`{ paths: [p] }` → `spawn_vscode_multi` → `code <path>`, no `-g`), its doc
comment already says "folders/files", and behaviour for both cases is
identical to today. That leaves `openContainerInVscode` on one command for all
three branches and lets `open_path_in_vscode` go away entirely rather than
linger as a second way to do the same thing.

`open_path_in_vscode` is then replaced by:

```rust
#[derive(Debug, serde::Deserialize)]
struct TerminalPathCandidate {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

#[tauri::command]
async fn open_terminal_path(
    candidates: Vec<TerminalPathCandidate>,
    base_dirs: Vec<String>,
) -> Result<(), String>;
```

Walk `candidates` in order, resolving each through the existing
`resolve_existing_terminal_path` (which already tries every base dir and
canonicalizes to non-verbatim form). The first that exists wins:

- candidate carries a line → `spawn_vscode(&resolved, line, column)`, as today
- otherwise → `open_with_default_app(&resolved)`

If none resolve, return the last error so the frontend logs something useful.

`open_with_default_app`:

- directory → `open_dir_in_file_manager`, extracted verbatim from the current
  `reveal_in_explorer` body (`explorer.exe` / `open` / `xdg-open`), which
  `reveal_in_explorer` then delegates to. Explicit, and it avoids relying on
  `SHOpenFolderAndSelectItems`, which reveals rather than opens.
- file → `tauri_plugin_opener::open_path(path, None::<&str>)`. The plugin is
  already a dependency and already enables the `open` crate's
  `shellexecute-on-windows` feature, so this is a real `ShellExecuteExW` with
  the default verb: correct association handling, and the OS's own "open with"
  picker for an unassociated type. It is called from Rust, so the
  `opener:allow-open-path` capability is not involved.

`open_url` and its `validate_http_url` + `rundll32` path are untouched —
restricting the shell to `http`/`https` there is deliberate.

## Tests

- **vitest** `apps/tauri-app/src/utils/terminalLinks.test.ts` — single-row
  detection (url, path, `:line:col`, trailing punctuation); soft-wrap merge
  across two rows; box hard-wrap merge with both candidates; and the rejection
  cases that make the heuristic worth having: a row ending well short of the
  wrap column, a continuation longer than the row above, a continuation
  starting with whitespace.
- **cargo** `lib.rs` tests — candidate order picks the stitched path when it
  exists, falls back to the fragment when it does not, and routes a line ref
  away from the default handler.
- **e2e** `tools/e2e/tests/e2e/specs/terminal-links.spec.ts` — print a path
  long enough to soft-wrap the pane, ctrl-click the second fragment, assert
  the dispatched event's `target` is the whole path.
