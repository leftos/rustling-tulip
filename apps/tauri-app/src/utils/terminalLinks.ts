export type TerminalLinkKind = "url" | "path";

/// One xterm buffer row as the detector sees it.
export interface TerminalRow {
  /// Row text padded to `cols` — xterm `translateToString(false)`.
  text: string;
  /// xterm `IBufferLine.isWrapped`: this row continues the one above.
  isWrapped: boolean;
}

/// One reading of a detected path: the whole stitched path, or a fragment of
/// it that stops at a stitch boundary.
export interface TerminalLinkCandidate {
  path: string;
  line: number | null;
  column: number | null;
}

export interface DetectedTerminalLink {
  kind: TerminalLinkKind;
  /// Full link text, stitching included.
  text: string;
  /// `candidates[0].path` — the longest reading.
  target: string;
  line: number | null;
  column: number | null;
  /// Open candidates, longest first. Length 1 unless the link crosses a hard
  /// wrap; each extra entry truncates at one stitch boundary, so an over-eager
  /// stitch degrades to the fragment that was actually on screen.
  candidates: TerminalLinkCandidate[];
  /// Offsets into the logical line the link was matched in.
  startIndex: number;
  endIndex: number;
  /// Offsets into the `rows` array; columns are 0-based, `endColumn` exclusive.
  startRow: number;
  startColumn: number;
  endRow: number;
  endColumn: number;
}

const URL_PATTERN = /\bhttps?:\/\/[^\s<>"'`{}|\\^]+/gi;
const PATH_PATTERN =
  /(^|[\s([{<])((?:[A-Za-z]:[\\/]|\\\\[^\s\\/:*?"<>|]+[\\/][^\s\\/:*?"<>|]+[\\/]|\/|\.{1,2}[\\/]|[A-Za-z0-9_.-]+[\\/])[^\s<>"'`|]*)/g;
const TRAILING_PUNCTUATION = /[),.;!?}\]]+$/;
const LINE_COLUMN_SUFFIX = /^(.*?)(?::([1-9]\d{0,6})(?::([1-9]\d{0,6}))?)$/;

/// Everything that can sit between a TUI box's edge and its content, plus the
/// pad spaces xterm reports for the unwritten tail of a row.
const FRAME_CHAR = /[\s│┃║╎╏┆┇┊┋▌▐|]/;
/// The frame characters that are an actual drawn border rather than padding.
const BORDER_CHAR = /[│┃║╎╏┆┇┊┋▌▐|]/;
/// A path run continues across a row break only if both sides of the break are
/// characters a path is made of.
const PATH_CONTINUATION = /[A-Za-z0-9_.\-\\/~$%+@#]/;
/// How far short of its wrap column a row may stop and still count as wrapped.
const STITCH_TOLERANCE = 4;
/// Hard stitches allowed in one logical line. Soft wraps are uncapped — the
/// buffer already calls those one line.
const STITCH_LIMIT = 4;

/// A piece of one row's text as it appears in an assembled logical line.
interface LineSegment {
  rowIndex: number;
  /// Column in `rows[rowIndex]` where `text` starts.
  columnOffset: number;
  text: string;
  /// This piece was joined on by a hard stitch, so its start is a boundary a
  /// candidate can truncate at.
  stitched: boolean;
}

export function detectTerminalLinks(text: string): DetectedTerminalLink[] {
  return detectSegmentLinks([
    { rowIndex: 0, columnOffset: 0, text, stitched: false },
  ]);
}

export function detectTerminalRowLinks(
  rows: TerminalRow[],
  cols: number,
): DetectedTerminalLink[] {
  const links: DetectedTerminalLink[] = [];
  for (const group of groupRows(rows, cols)) {
    links.push(...detectSegmentLinks(group));
  }
  return links;
}

/// Whether `below` continues `above` across a hard line break — a real newline
/// a TUI emitted rather than a soft wrap the buffer flagged. Exported because
/// `Terminal.tsx`'s row-window walk applies the same test.
export function canStitch(
  above: TerminalRow,
  below: TerminalRow,
  cols: number,
): boolean {
  if (below.isWrapped) return false;
  const a = stripFrame(above.text);
  const b = stripFrame(below.text);
  if (a.content.length === 0 || b.content.length === 0) return false;
  const tail = a.content[a.content.length - 1] ?? "";
  const head = b.content[0] ?? "";
  if (!PATH_CONTINUATION.test(tail) || !PATH_CONTINUATION.test(head)) {
    return false;
  }
  // A TUI only breaks a row because it ran out of width, so a row that stops
  // well short of its wrap column ended rather than wrapped.
  if (a.end < wrapColumn(above.text, cols) - STITCH_TOLERANCE) return false;
  // A full row wraps; a row longer than the one above it is not its tail.
  return b.content.length <= a.content.length;
}

/// Split a row into the content a link can live in and the frame around it.
/// The trailing run of frame characters — a box's right border plus xterm's
/// pad spaces — always goes. The leading run only goes when it contains a
/// border glyph, which makes it a box's left edge and its padding; a bare run
/// of spaces is indentation that belongs to the content, and keeping it is
/// what lets an indented row read as a fresh line rather than a continuation.
function stripFrame(text: string): {
  content: string;
  start: number;
  end: number;
} {
  let end = text.length;
  while (end > 0 && FRAME_CHAR.test(text[end - 1] ?? "")) end -= 1;
  let start = 0;
  while (start < end && FRAME_CHAR.test(text[start] ?? "")) start += 1;
  if (!BORDER_CHAR.test(text.slice(0, start))) start = 0;
  return { content: text.slice(start, end), start, end };
}

/// The column a row would have wrapped at: a box's right border if it has one,
/// otherwise the terminal width.
function wrapColumn(text: string, cols: number): number {
  let end = text.length;
  while (end > 0 && /\s/.test(text[end - 1] ?? "")) end -= 1;
  if (end > 0 && BORDER_CHAR.test(text[end - 1] ?? "")) return end - 1;
  return cols;
}

function groupRows(rows: TerminalRow[], cols: number): LineSegment[][] {
  const groups: LineSegment[][] = [];
  let current: LineSegment[] = [];
  let stitches = 0;
  rows.forEach((row, index) => {
    const previous = index > 0 ? rows[index - 1] : undefined;
    if (current.length === 0) {
      current.push(wholeRowSegment(index, row));
      stitches = 0;
      return;
    }
    if (row.isWrapped) {
      current.push(wholeRowSegment(index, row));
      return;
    }
    if (previous && stitches < STITCH_LIMIT && canStitch(previous, row, cols)) {
      // The row above keeps only its framed content: its border and pad
      // spaces would otherwise land between the two halves of the path.
      truncateToContent(current, rows);
      const framed = stripFrame(row.text);
      current.push({
        rowIndex: index,
        columnOffset: framed.start,
        text: framed.content,
        stitched: true,
      });
      stitches += 1;
      return;
    }
    groups.push(current);
    current = [wholeRowSegment(index, row)];
    stitches = 0;
  });
  if (current.length > 0) groups.push(current);
  return groups;
}

function wholeRowSegment(index: number, row: TerminalRow): LineSegment {
  return { rowIndex: index, columnOffset: 0, text: row.text, stitched: false };
}

function truncateToContent(segments: LineSegment[], rows: TerminalRow[]): void {
  const last = segments[segments.length - 1];
  if (!last) return;
  const row = rows[last.rowIndex];
  if (!row) return;
  const keep = Math.max(0, stripFrame(row.text).end - last.columnOffset);
  if (keep < last.text.length) {
    last.text = last.text.slice(0, keep);
  }
}

function detectSegmentLinks(segments: LineSegment[]): DetectedTerminalLink[] {
  const text = segments.map((segment) => segment.text).join("");
  const links: DetectedTerminalLink[] = [];

  for (const match of text.matchAll(URL_PATTERN)) {
    const startIndex = match.index ?? 0;
    const trimmed = trimLinkCandidate(match[0]);
    if (!trimmed) continue;
    links.push(buildLink("url", startIndex, trimmed, segments));
  }

  for (const match of text.matchAll(PATH_PATTERN)) {
    const raw = match[2] ?? "";
    const prefix = match[1] ?? "";
    const startIndex = (match.index ?? 0) + prefix.length;
    const trimmed = trimLinkCandidate(raw);
    if (!trimmed || overlapsExistingLink(startIndex, startIndex + trimmed.length, links)) {
      continue;
    }
    links.push(buildLink("path", startIndex, trimmed, segments));
  }

  return links.sort((a, b) => a.startIndex - b.startIndex);
}

function buildLink(
  kind: TerminalLinkKind,
  startIndex: number,
  text: string,
  segments: LineSegment[],
): DetectedTerminalLink {
  const endIndex = startIndex + text.length;
  const start = locate(segments, startIndex);
  const end = locate(segments, endIndex - 1);
  // A browser cannot verify a URL the way the filesystem verifies a path, so
  // there is nothing to fall back to: one candidate, always.
  const candidates =
    kind === "url"
      ? [{ path: text, line: null, column: null }]
      : buildCandidates(text, startIndex, endIndex, segments);
  const primary = candidates[0] ?? { path: text, line: null, column: null };
  return {
    kind,
    text,
    target: primary.path,
    line: primary.line,
    column: primary.column,
    candidates,
    startIndex,
    endIndex,
    startRow: start.row,
    startColumn: start.column,
    endRow: end.row,
    endColumn: end.column + 1,
  };
}

function buildCandidates(
  text: string,
  startIndex: number,
  endIndex: number,
  segments: LineSegment[],
): TerminalLinkCandidate[] {
  const readings = [text];
  let offset = 0;
  const boundaries: number[] = [];
  for (const segment of segments) {
    if (segment.stitched && offset > startIndex && offset < endIndex) {
      boundaries.push(offset);
    }
    offset += segment.text.length;
  }
  for (const boundary of boundaries.slice().reverse()) {
    readings.push(text.slice(0, boundary - startIndex));
  }

  const candidates: TerminalLinkCandidate[] = [];
  for (const reading of readings) {
    if (!reading) continue;
    const split = splitPathPosition(reading);
    if (!split.path) continue;
    const duplicate = candidates.some(
      (candidate) =>
        candidate.path === split.path &&
        candidate.line === split.line &&
        candidate.column === split.column,
    );
    if (duplicate) continue;
    candidates.push(split);
  }
  return candidates;
}

function locate(
  segments: LineSegment[],
  index: number,
): { row: number; column: number } {
  let offset = 0;
  for (const segment of segments) {
    const next = offset + segment.text.length;
    if (index < next) {
      return {
        row: segment.rowIndex,
        column: segment.columnOffset + (index - offset),
      };
    }
    offset = next;
  }
  const last = segments[segments.length - 1];
  if (!last) return { row: 0, column: Math.max(index, 0) };
  return { row: last.rowIndex, column: last.columnOffset + last.text.length };
}

function trimLinkCandidate(value: string): string {
  let next = value;
  while (TRAILING_PUNCTUATION.test(next)) {
    next = next.replace(TRAILING_PUNCTUATION, "");
  }
  if (next.endsWith(":") && !/^[A-Za-z]:$/.test(next)) {
    next = next.slice(0, -1);
  }
  return next;
}

function splitPathPosition(value: string): TerminalLinkCandidate {
  const match = LINE_COLUMN_SUFFIX.exec(value);
  if (!match) {
    return { path: value, line: null, column: null };
  }
  return {
    path: match[1] ?? value,
    line: Number(match[2]),
    column: match[3] ? Number(match[3]) : null,
  };
}

function overlapsExistingLink(
  startIndex: number,
  endIndex: number,
  links: DetectedTerminalLink[],
): boolean {
  return links.some(
    (link) => startIndex < link.endIndex && endIndex > link.startIndex,
  );
}
