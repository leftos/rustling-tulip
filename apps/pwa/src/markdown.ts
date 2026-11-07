/**
 * The agent's answers as Markdown blocks and spans: plain data the session
 * screen's view turns into DOM nodes. A hand-written parser for what the
 * desktop's `md_blocks` draws: headings, paragraphs, emphasis, strong,
 * strikethrough, inline code, fenced and indented code, block quotes, lists
 * with task items, tables with alignment, rules and links. A single line
 * break is a line break, as in the desktop's Chat answers.
 *
 * Agent text is untrusted, so the tree keeps all of it as text: raw HTML stays
 * literal, an image is its alt text, and a link carries an `href` only for an
 * `http:` or `https:` URL. Hostile text cannot make the parse slow either: each
 * inline pass indexes its text once, so no opener rescans the text after it.
 * Pure: no DOM, no globals.
 */

/** A table column's alignment; null when the delimiter row names none. */
export type MdAlign = "left" | "center" | "right" | null;

/** A heading's level. */
export type MdHeadingLevel = 1 | 2 | 3 | 4 | 5 | 6;

/** One run of text with the same marks, or a line break. */
export interface MdSpan {
  readonly text: string;
  readonly strong?: true;
  readonly em?: true;
  readonly strike?: true;
  readonly code?: true;
  /** The link's URL, only ever an `http:` or `https:` one. */
  readonly href?: string;
  /** A line break; its text is empty. */
  readonly br?: true;
}

/** One list item: its task box (null for a plain item) and its blocks. */
export interface MdListItem {
  readonly task: boolean | null;
  readonly blocks: readonly MdBlock[];
}

/** A block-level element. */
export type MdBlock =
  | { readonly kind: "heading"; readonly level: MdHeadingLevel; readonly spans: readonly MdSpan[] }
  | { readonly kind: "paragraph"; readonly spans: readonly MdSpan[] }
  | { readonly kind: "code"; readonly lang: string; readonly text: string }
  | { readonly kind: "quote"; readonly blocks: readonly MdBlock[] }
  | {
      readonly kind: "list";
      readonly ordered: boolean;
      /** The number an ordered list begins at; 1 for a bullet list. */
      readonly start: number;
      readonly items: readonly MdListItem[];
    }
  | {
      readonly kind: "table";
      readonly align: readonly MdAlign[];
      /** One cell per column. */
      readonly header: readonly (readonly MdSpan[])[];
      /** Each row has one cell per column: a short row is padded with empty cells. */
      readonly rows: readonly (readonly (readonly MdSpan[])[])[];
    }
  | { readonly kind: "rule" };

/**
 * How deep quotes, lists and inline marks nest; markers deeper than this stay
 * literal text, so hostile text cannot run the parser out of stack.
 */
export const MAX_NESTING = 32;

/**
 * A tally of the characters and index probes the inline scanner examines, so
 * a test can pin the parse's work as linear in the text's length.
 */
export interface ParseWork {
  count: number;
}

/** The blocks of a Markdown text; empty text has none. */
export function parseMarkdown(text: string, work: ParseWork = { count: 0 }): MdBlock[] {
  const lines = text.split(/\r\n|\r|\n/).map(expandTabs);
  return parseBlocks({ work }, lines, 0);
}

/** What every step of one parse shares. */
interface Context {
  readonly work: ParseWork;
}

const FENCE_OPEN = /^( {0,3})(`{3,}|~{3,})(.*)$/;
const FENCE_CLOSE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/;
const RULE = /^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$/;
const SETEXT = /^ {0,3}(?:=+|-+)[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const LIST = /^( {0,3})(?:([-*+])|(\d{1,9})([.)]))(?:([ \t]+)(.*))?$/;
const TASK = /^\[([ xX])\](?:[ \t]+|$)/;
const DELIMITER = /^:?-+:?$/;
const BLANK = /^[ \t]*$/;
const WHITESPACE = /\s/;
const WORD = /[\p{L}\p{N}]/u;
const PUNCTUATION = /^[!-/:-@[-`{-~]$/;
const ESCAPED = /\\([!-/:-@[-`{-~])/g;
const AUTOLINK_SCHEME = /^<[A-Za-z][A-Za-z0-9+.-]{1,31}:/;
const AUTOLINK_STOP = /[\s<>]/;
const SAFE_SCHEME = /^https?:\/\//i;

/** The longest link destination, and the most space around it, that still makes a link. */
const URL_LIMIT = 2048;

/** The longest autolink scheme, with its `<` and `:`. */
const SCHEME_SPAN = 34;

/** A line with its leading tabs turned into spaces, at tab stops of four. */
function expandTabs(line: string): string {
  let indent = "";
  let at = 0;
  for (; at < line.length; at += 1) {
    const ch = line.charAt(at);
    if (ch === " ") {
      indent += " ";
    } else if (ch === "\t") {
      indent += " ".repeat(4 - (indent.length % 4));
    } else {
      break;
    }
  }
  return at === 0 ? line : indent + line.slice(at);
}

function lineAt(lines: readonly string[], at: number): string {
  return lines[at] ?? "";
}

function group(match: RegExpExecArray, index: number): string {
  return match[index] ?? "";
}

function leadingSpaces(line: string): number {
  return line.length - line.trimStart().length;
}

function isBlank(line: string | undefined): boolean {
  return BLANK.test(line ?? "");
}

function stripIndent(line: string, columns: number): string {
  return line.slice(Math.min(columns, leadingSpaces(line)));
}

function dropTrailingBlanks(body: string[]): void {
  while (body.length > 0 && isBlank(body.at(-1))) {
    body.pop();
  }
}

/** A block rule: where the block it reads at `at` ends, or -1 when the line starts none of its kind. */
type BlockRule = (
  context: Context,
  lines: readonly string[],
  at: number,
  depth: number,
  out: MdBlock[],
) => number;

function parseBlocks(context: Context, lines: readonly string[], depth: number): MdBlock[] {
  const blocks: MdBlock[] = [];
  let at = 0;
  while (at < lines.length) {
    at = isBlank(lineAt(lines, at)) ? at + 1 : readBlock(context, lines, at, depth, blocks);
  }
  return blocks;
}

/** The first block rule that reads the line; a paragraph reads any line. */
function readBlock(
  context: Context,
  lines: readonly string[],
  at: number,
  depth: number,
  out: MdBlock[],
): number {
  for (const rule of BLOCK_RULES) {
    const next = rule(context, lines, at, depth, out);
    if (next >= 0) {
      return next;
    }
  }
  return at + 1;
}

/** Whether a line starts a block, so it ends the paragraph above it. */
function startsBlock(line: string, nested: boolean): boolean {
  if (fenceOpen(line) !== null || headingAt(line) !== null || RULE.test(line)) {
    return true;
  }
  return nested && (QUOTE.test(line) || interruptsWithList(line));
}

/** A list item that may cut a paragraph short: one with text, and an ordered one only from 1. */
function interruptsWithList(line: string): boolean {
  const marker = listMarker(line);
  return marker !== null && marker.first.trim() !== "" && (!marker.ordered || marker.start === 1);
}

interface Fence {
  readonly indent: number;
  readonly char: string;
  readonly length: number;
  readonly lang: string;
}

function fenceOpen(line: string): Fence | null {
  const match = FENCE_OPEN.exec(line);
  if (match === null) {
    return null;
  }
  const marks = group(match, 2);
  const info = group(match, 3).trim();
  if (marks.startsWith("`") && info.includes("`")) {
    return null;
  }
  return {
    indent: group(match, 1).length,
    char: marks.charAt(0),
    length: marks.length,
    lang: info.split(/\s+/)[0] ?? "",
  };
}

function closesFence(line: string, fence: Fence): boolean {
  const match = FENCE_CLOSE.exec(line);
  const marks = match === null ? "" : group(match, 1);
  return marks.startsWith(fence.char) && marks.length >= fence.length;
}

/** A fenced code block; one never closed runs to the end of the text. */
function fencedCode(
  _context: Context,
  lines: readonly string[],
  at: number,
  _depth: number,
  out: MdBlock[],
): number {
  const fence = fenceOpen(lineAt(lines, at));
  if (fence === null) {
    return -1;
  }
  const body: string[] = [];
  let next = at + 1;
  while (next < lines.length) {
    const line = lineAt(lines, next);
    next += 1;
    if (closesFence(line, fence)) {
      break;
    }
    body.push(stripIndent(line, fence.indent));
  }
  out.push({ kind: "code", lang: fence.lang, text: body.join("\n") });
  return next;
}

/** An ATX heading's level and text, read by hand so a long line cannot make a regex backtrack. */
function headingAt(line: string): { level: MdHeadingLevel; text: string } | null {
  const start = leadingSpaces(line);
  let end = start;
  while (line.charAt(end) === "#") {
    end += 1;
  }
  const level = end - start;
  if (start > 3 || level < 1 || level > 6 || !isHeadingGap(line.charAt(end))) {
    return null;
  }
  return { level: level as MdHeadingLevel, text: headingText(line.slice(end).trim()) };
}

function isHeadingGap(ch: string): boolean {
  return ch === "" || ch === " " || ch === "\t";
}

/** A heading's text without its closing run of `#`, which needs a space before it. */
function headingText(text: string): string {
  let end = text.length;
  while (end > 0 && text.charAt(end - 1) === "#") {
    end -= 1;
  }
  if (end === 0) {
    return "";
  }
  const before = text.charAt(end - 1);
  return end < text.length && (before === " " || before === "\t")
    ? text.slice(0, end).trimEnd()
    : text;
}

function atxHeading(
  context: Context,
  lines: readonly string[],
  at: number,
  _depth: number,
  out: MdBlock[],
): number {
  const heading = headingAt(lineAt(lines, at));
  if (heading === null) {
    return -1;
  }
  out.push({ kind: "heading", level: heading.level, spans: parseInline(context, heading.text) });
  return at + 1;
}

function thematicBreak(
  _context: Context,
  lines: readonly string[],
  at: number,
  _depth: number,
  out: MdBlock[],
): number {
  if (!RULE.test(lineAt(lines, at))) {
    return -1;
  }
  out.push({ kind: "rule" });
  return at + 1;
}

/** A block quote: its `>` lines, and the unmarked lines that continue its last paragraph. */
function blockQuote(
  context: Context,
  lines: readonly string[],
  at: number,
  depth: number,
  out: MdBlock[],
): number {
  if (depth >= MAX_NESTING || !QUOTE.test(lineAt(lines, at))) {
    return -1;
  }
  const inner: string[] = [];
  let next = at;
  while (next < lines.length && takeQuoteLine(lineAt(lines, next), inner)) {
    next += 1;
  }
  out.push({ kind: "quote", blocks: parseBlocks(context, inner, depth + 1) });
  return next;
}

function takeQuoteLine(line: string, inner: string[]): boolean {
  const marked = QUOTE.exec(line);
  if (marked !== null) {
    inner.push(group(marked, 1));
    return true;
  }
  if (isBlank(line) || isBlank(inner.at(-1)) || startsBlock(line, true)) {
    return false;
  }
  inner.push(line);
  return true;
}

interface Marker {
  readonly ordered: boolean;
  /** The bullet character, or an ordered marker's `.` or `)`: a list's items share it. */
  readonly kind: string;
  readonly start: number;
  /** The column the item's text starts at; its continuation lines are indented this far. */
  readonly contentIndent: number;
  /** The item's first line, without the marker. */
  readonly first: string;
}

function listMarker(line: string): Marker | null {
  const match = LIST.exec(line);
  if (match === null) {
    return null;
  }
  const digits = group(match, 3);
  const ordered = digits !== "";
  const marker = ordered ? digits + group(match, 4) : group(match, 2);
  return {
    ordered,
    kind: ordered ? group(match, 4) : marker,
    start: ordered ? Number(digits) : 1,
    ...itemText(group(match, 1).length + marker.length, group(match, 5), group(match, 6)),
  };
}

/** Where an item's text starts and its first line: a gap over four spaces starts indented code. */
function itemText(
  width: number,
  gap: string,
  rest: string,
): { contentIndent: number; first: string } {
  if (rest === "") {
    return { contentIndent: width + 1, first: "" };
  }
  if (gap.length > 4) {
    return { contentIndent: width + 1, first: gap.slice(1) + rest };
  }
  return { contentIndent: width + gap.length, first: rest };
}

function list(
  context: Context,
  lines: readonly string[],
  at: number,
  depth: number,
  out: MdBlock[],
): number {
  const first = depth < MAX_NESTING ? listMarker(lineAt(lines, at)) : null;
  if (first === null) {
    return -1;
  }
  const items: MdListItem[] = [];
  let next = at;
  let marker: Marker | null = first;
  while (marker !== null) {
    const body = [marker.first];
    next = itemBody(lines, next + 1, marker, body);
    items.push(listItem(context, body, depth));
    marker = sibling(lineAt(lines, next), first);
  }
  out.push({ kind: "list", ordered: first.ordered, start: first.start, items });
  return next;
}

/** Reads an item's lines after its first into `body`; where the item ends. */
function itemBody(lines: readonly string[], from: number, marker: Marker, body: string[]): number {
  let next = from;
  while (next < lines.length && takeItemLine(lineAt(lines, next), marker, body)) {
    next += 1;
  }
  dropTrailingBlanks(body);
  return next;
}

function takeItemLine(line: string, marker: Marker, body: string[]): boolean {
  if (isBlank(line)) {
    body.push("");
    return true;
  }
  if (leadingSpaces(line) >= marker.contentIndent) {
    body.push(line.slice(marker.contentIndent));
    return true;
  }
  if (isBlank(body.at(-1)) || listMarker(line) !== null || startsBlock(line, true)) {
    return false;
  }
  body.push(line.trimStart());
  return true;
}

/** The next item's marker when the line continues the same list, else null. */
function sibling(line: string, first: Marker): Marker | null {
  const next = RULE.test(line) ? null : listMarker(line);
  return next !== null && next.ordered === first.ordered && next.kind === first.kind ? next : null;
}

function listItem(context: Context, body: readonly string[], depth: number): MdListItem {
  const head = lineAt(body, 0);
  const task = TASK.exec(head);
  if (task === null) {
    return { task: null, blocks: parseBlocks(context, body, depth + 1) };
  }
  const rest = [head.slice(task[0].length), ...body.slice(1)];
  return { task: group(task, 1) !== " ", blocks: parseBlocks(context, rest, depth + 1) };
}

function indentedCode(
  _context: Context,
  lines: readonly string[],
  at: number,
  _depth: number,
  out: MdBlock[],
): number {
  if (leadingSpaces(lineAt(lines, at)) < 4) {
    return -1;
  }
  const body: string[] = [];
  let next = at;
  while (next < lines.length && takeCodeLine(lineAt(lines, next), body)) {
    next += 1;
  }
  dropTrailingBlanks(body);
  out.push({ kind: "code", lang: "", text: body.join("\n") });
  return next;
}

function takeCodeLine(line: string, body: string[]): boolean {
  if (isBlank(line)) {
    body.push("");
    return true;
  }
  if (leadingSpaces(line) < 4) {
    return false;
  }
  body.push(line.slice(4));
  return true;
}

/**
 * A table's alignments when a pipe table starts at this line, else null. A
 * one-column table needs a pipe in its delimiter row, so `a |` over `---`
 * stays a setext heading.
 */
function tableAlign(lines: readonly string[], at: number): MdAlign[] | null {
  const header = lineAt(lines, at);
  const delimiter = lines[at + 1];
  if (delimiter === undefined || !header.includes("|")) {
    return null;
  }
  const cells = splitCells(delimiter);
  const fits =
    cells.every((cell) => DELIMITER.test(cell)) &&
    cells.length === splitCells(header).length &&
    (cells.length >= 2 || delimiter.includes("|"));
  return fits ? cells.map(cellAlign) : null;
}

function cellAlign(cell: string): MdAlign {
  const left = cell.startsWith(":");
  const right = cell.endsWith(":");
  if (left && right) {
    return "center";
  }
  if (left) {
    return "left";
  }
  return right ? "right" : null;
}

function table(
  context: Context,
  lines: readonly string[],
  at: number,
  _depth: number,
  out: MdBlock[],
): number {
  const align = tableAlign(lines, at);
  if (align === null) {
    return -1;
  }
  const header = cellsOf(context, lineAt(lines, at), align.length);
  const rows: MdSpan[][][] = [];
  let next = at + 2;
  while (next < lines.length && isTableRow(lineAt(lines, next))) {
    rows.push(cellsOf(context, lineAt(lines, next), align.length));
    next += 1;
  }
  out.push({ kind: "table", align, header, rows });
  return next;
}

function isTableRow(line: string): boolean {
  return !isBlank(line) && !startsBlock(line, true);
}

/** A row's cells, padded or cut to the table's width. */
function cellsOf(context: Context, line: string, width: number): MdSpan[][] {
  const cells = splitCells(line);
  return Array.from({ length: width }, (_, at) => parseInline(context, cells[at] ?? ""));
}

/** A table row's cells, split at each pipe a backslash does not escape. */
function splitCells(line: string): string[] {
  let row = line.trim();
  if (row.startsWith("|")) {
    row = row.slice(1);
  }
  if (row.endsWith("|") && !row.endsWith("\\|")) {
    row = row.slice(0, -1);
  }
  const cells: string[] = [];
  let cell = "";
  for (let at = 0; at < row.length; at += 1) {
    const ch = row.charAt(at);
    if (ch === "\\" && row.charAt(at + 1) === "|") {
      cell += "|";
      at += 1;
    } else if (ch === "|") {
      cells.push(cell.trim());
      cell = "";
    } else {
      cell += ch;
    }
  }
  cells.push(cell.trim());
  return cells;
}

/** A paragraph, or a setext heading when an underline ends it. */
function paragraph(
  context: Context,
  lines: readonly string[],
  at: number,
  depth: number,
  out: MdBlock[],
): number {
  const text = [lineAt(lines, at).trim()];
  let next = at + 1;
  while (next < lines.length && continuesParagraph(lines, next, depth)) {
    text.push(lineAt(lines, next).trim());
    next += 1;
  }
  const spans = parseInline(context, text.join("\n"));
  const underline = lineAt(lines, next);
  if (next < lines.length && SETEXT.test(underline)) {
    out.push({ kind: "heading", level: underline.trim().startsWith("=") ? 1 : 2, spans });
    return next + 1;
  }
  out.push({ kind: "paragraph", spans });
  return next;
}

function continuesParagraph(lines: readonly string[], at: number, depth: number): boolean {
  const line = lineAt(lines, at);
  return (
    !isBlank(line) &&
    !SETEXT.test(line) &&
    !startsBlock(line, depth < MAX_NESTING) &&
    tableAlign(lines, at) === null
  );
}

const BLOCK_RULES: readonly BlockRule[] = [
  fencedCode,
  atxHeading,
  thematicBreak,
  blockQuote,
  list,
  indentedCode,
  table,
  paragraph,
];

/** The marks a run of text carries, apart from code. */
interface Marks {
  readonly strong?: true;
  readonly em?: true;
  readonly strike?: true;
  readonly href?: string;
}

const NO_MARKS: Marks = {};

/** The spans being built: adjacent text with the same marks joins one span. */
class SpanList {
  readonly spans: MdSpan[] = [];

  text(text: string, marks: Marks, code = false): void {
    if (text === "") {
      return;
    }
    const last = this.spans.at(-1);
    if (last !== undefined && last.br !== true && sameMarks(last, marks, code)) {
      this.spans[this.spans.length - 1] = { ...last, text: last.text + text };
      return;
    }
    this.spans.push(makeSpan(text, marks, code));
  }

  br(): void {
    this.spans.push({ text: "", br: true });
  }
}

function makeSpan(text: string, marks: Marks, code: boolean): MdSpan {
  return {
    text,
    ...(marks.strong === true ? { strong: true as const } : {}),
    ...(marks.em === true ? { em: true as const } : {}),
    ...(marks.strike === true ? { strike: true as const } : {}),
    ...(code ? { code: true as const } : {}),
    ...(marks.href === undefined ? {} : { href: marks.href }),
  };
}

function sameMarks(span: MdSpan, marks: Marks, code: boolean): boolean {
  return (
    span.strong === marks.strong &&
    span.em === marks.em &&
    span.strike === marks.strike &&
    (span.code === true) === code &&
    span.href === marks.href
  );
}

function parseInline(context: Context, text: string): MdSpan[] {
  const out = new SpanList();
  inline(context, text, NO_MARKS, 0, out);
  return out.spans;
}

function inline(context: Context, text: string, marks: Marks, depth: number, out: SpanList): void {
  new InlineScan(context, text, marks, depth, out).run();
}

/** A code span's opening run: its length and where its closing run starts. */
interface CodeSpan {
  readonly length: number;
  readonly close: number;
}

/**
 * Where each closing delimiter run can close a `length`-long opener: the
 * starts of runs of exactly that length, and for longer runs where their last
 * `length` characters start. Both are in text order.
 */
interface Closers {
  readonly exact: number[];
  readonly longer: number[];
}

/** Where a `(`'s destination run ends: its matching `)`, else the whitespace or text end that ends the run. */
interface ParenRun {
  readonly end: number;
}

/** One pass over a text's inline content, with the index it takes once up front. */
class InlineScan {
  /** Plain text not yet added as a span. */
  plain = "";
  readonly code: ReadonlyMap<number, CodeSpan>;
  readonly brackets: ReadonlyMap<number, number>;
  readonly closers: ReadonlyMap<string, Closers>;
  readonly parens: ReadonlyMap<number, ParenRun>;
  /** For each character looked for, the last search: where it started and what it found. */
  private readonly found = new Map<string, { readonly from: number; readonly at: number }>();

  constructor(
    readonly context: Context,
    readonly text: string,
    readonly marks: Marks,
    readonly depth: number,
    readonly out: SpanList,
  ) {
    this.code = codeSpans(context, text);
    const index = indexText(context, text, this.code);
    this.brackets = index.brackets;
    this.closers = index.closers;
    this.parens = parenRuns(context, text);
  }

  run(): void {
    let at = 0;
    while (at < this.text.length) {
      this.context.work.count += 1;
      at = this.step(at);
    }
    this.flush();
  }

  /** Reads what starts at `at`; where it ends. */
  private step(at: number): number {
    const ch = this.text.charAt(at);
    const handler = HANDLERS.get(ch);
    const end = handler === undefined ? -1 : handler(this, at);
    if (end >= 0) {
      return end;
    }
    this.plain += ch;
    return at + 1;
  }

  flush(): void {
    this.out.text(this.plain, this.marks);
    this.plain = "";
  }

  /** Where the run closing a `length`-long `ch` opener ending at `from` starts; null when none does. */
  closer(ch: string, from: number, length: number): number | null {
    const closers = this.closers.get(`${ch}${length}`);
    if (closers === undefined) {
      return null;
    }
    const exact = firstFrom(closers.exact, from, this.context.work);
    return exact ?? firstFrom(closers.longer, from, this.context.work) ?? null;
  }

  /** The next `ch` at or after `from`, or the text's length; amortised over searches that move forward. */
  nextIndex(ch: string, from: number): number {
    const known = this.found.get(ch);
    if (known !== undefined && known.from <= from && from <= known.at) {
      return known.at;
    }
    let at = from;
    while (at < this.text.length && this.text.charAt(at) !== ch) {
      this.context.work.count += 1;
      at += 1;
    }
    this.found.set(ch, { from, at });
    return at;
  }
}

/** The first value at or after `from` in a sorted list, by binary search. */
function firstFrom(sorted: readonly number[], from: number, work: ParseWork): number | undefined {
  let low = 0;
  let high = sorted.length;
  while (low < high) {
    work.count += 1;
    const middle = (low + high) >> 1;
    if ((sorted[middle] ?? 0) < from) {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  return sorted[low];
}

/** Where the run of the character at `at` ends. */
function runEnd(text: string, at: number): number {
  const ch = text.charAt(at);
  let end = at;
  while (end < text.length && text.charAt(end) === ch) {
    end += 1;
  }
  return end;
}

/**
 * Each backtick run that opens a code span, paired with the next run of the
 * same length. A backslash escapes a backtick only outside a code span: inside
 * one, every character is literal, so the closing run is found among the raw runs.
 */
function codeSpans(context: Context, text: string): Map<number, CodeSpan> {
  const raw = rawRuns(context, text);
  const pairs = new Map<number, CodeSpan>();
  let at = 0;
  while (at < text.length) {
    context.work.count += 1;
    at = codeStep(context, text, at, raw, pairs);
  }
  return pairs;
}

function codeStep(
  context: Context,
  text: string,
  at: number,
  raw: ReadonlyMap<number, number[]>,
  pairs: Map<number, CodeSpan>,
): number {
  const ch = text.charAt(at);
  if (ch === "\\") {
    return at + 2;
  }
  if (ch !== "`") {
    return at + 1;
  }
  const end = runEnd(text, at);
  const length = end - at;
  const close = firstFrom(raw.get(length) ?? [], end, context.work);
  if (close === undefined) {
    return end;
  }
  pairs.set(at, { length, close });
  return close + length;
}

/** Every maximal backtick run's start, by its length, backslashes ignored. */
function rawRuns(context: Context, text: string): Map<number, number[]> {
  const runs = new Map<number, number[]>();
  let at = 0;
  while (at < text.length) {
    context.work.count += 1;
    if (text.charAt(at) === "`") {
      const end = runEnd(text, at);
      const same = runs.get(end - at) ?? [];
      same.push(at);
      runs.set(end - at, same);
      at = end;
    } else {
      at += 1;
    }
  }
  return runs;
}

const DELIMITERS = new Set(["*", "_", "~"]);

/** Each `[` and the `]` that balances it, and each closing delimiter run, outside escapes and code spans. */
function indexText(
  context: Context,
  text: string,
  code: ReadonlyMap<number, CodeSpan>,
): { brackets: Map<number, number>; closers: Map<string, Closers> } {
  const brackets = new Map<number, number>();
  const closers = new Map<string, Closers>();
  const open: number[] = [];
  let at = 0;
  while (at < text.length) {
    context.work.count += 1;
    const span = code.get(at);
    if (span === undefined) {
      at = indexStep(text, at, open, brackets, closers);
    } else {
      at = span.close + span.length;
    }
  }
  return { brackets, closers };
}

function indexStep(
  text: string,
  at: number,
  open: number[],
  brackets: Map<number, number>,
  closers: Map<string, Closers>,
): number {
  const ch = text.charAt(at);
  if (ch === "\\") {
    return at + 2;
  }
  if (DELIMITERS.has(ch)) {
    return addCloser(text, at, closers);
  }
  if (ch === "[") {
    open.push(at);
  } else if (ch === "]") {
    const start = open.pop();
    if (start !== undefined) {
      brackets.set(start, at);
    }
  }
  return at + 1;
}

/** Files a delimiter run under every opener length it can close; where the run ends. */
function addCloser(text: string, at: number, closers: Map<string, Closers>): number {
  const end = runEnd(text, at);
  if (!closesRun(text, at, end)) {
    return end;
  }
  const ch = text.charAt(at);
  const run = end - at;
  for (let length = 1; length <= Math.min(run, 3); length += 1) {
    const key = `${ch}${length}`;
    const entry = closers.get(key) ?? { exact: [], longer: [] };
    if (run === length) {
      entry.exact.push(at);
    } else {
      entry.longer.push(end - length);
    }
    closers.set(key, entry);
  }
  return end;
}

/** Whether a delimiter run can close: no space before it, and an underscore run no letter after it. */
function closesRun(text: string, at: number, end: number): boolean {
  const before = text.charAt(at - 1);
  if (before === "" || WHITESPACE.test(before)) {
    return false;
  }
  return text.charAt(at) !== "_" || !WORD.test(text.charAt(end));
}

/**
 * Where each `(`'s destination run ends: the `)` that balances it, or the
 * whitespace (or text end) that ends its run first. Escaped characters are skipped.
 */
function parenRuns(context: Context, text: string): Map<number, ParenRun> {
  const runs = new Map<number, ParenRun>();
  const open: number[] = [];
  let at = 0;
  while (at < text.length) {
    context.work.count += 1;
    at = parenStep(text, at, open, runs);
  }
  endRuns(open, text.length, runs);
  return runs;
}

function parenStep(text: string, at: number, open: number[], runs: Map<number, ParenRun>): number {
  const ch = text.charAt(at);
  if (ch === "\\") {
    return at + 2;
  }
  if (ch === "(") {
    open.push(at);
  } else if (ch === ")") {
    const start = open.pop();
    if (start !== undefined) {
      runs.set(start, { end: at });
    }
  } else if (WHITESPACE.test(ch)) {
    endRuns(open, at, runs);
  }
  return at + 1;
}

function endRuns(open: number[], at: number, runs: Map<number, ParenRun>): void {
  for (const start of open) {
    runs.set(start, { end: at });
  }
  open.length = 0;
}

/** What an inline character starts: where it ends, or -1 to take the character as text. */
type Handler = (scan: InlineScan, at: number) => number;

function backslash(scan: InlineScan, at: number): number {
  const next = scan.text.charAt(at + 1);
  if (next === "\n") {
    return at + 1;
  }
  if (!PUNCTUATION.test(next)) {
    return -1;
  }
  scan.plain += next;
  return at + 2;
}

function newline(scan: InlineScan, at: number): number {
  scan.flush();
  scan.out.br();
  return at + 1;
}

function backtick(scan: InlineScan, at: number): number {
  const span = scan.code.get(at);
  if (span === undefined) {
    const end = runEnd(scan.text, at);
    scan.plain += scan.text.slice(at, end);
    return end;
  }
  scan.flush();
  scan.out.text(codeText(scan.text.slice(at + span.length, span.close)), scan.marks, true);
  return span.close + span.length;
}

/** A code span's text: line breaks as spaces, and one padding space trimmed from each side. */
function codeText(raw: string): string {
  const text = raw.replaceAll("\n", " ");
  if (text.length >= 2 && text.startsWith(" ") && text.endsWith(" ") && text.trim() !== "") {
    return text.slice(1, -1);
  }
  return text;
}

/** An autolink `<scheme:…>`: a link when safe, else its URL as text. */
function autolink(scan: InlineScan, at: number): number {
  const { text } = scan;
  const scheme = AUTOLINK_SCHEME.exec(text.slice(at, at + SCHEME_SPAN));
  if (scheme === null) {
    return -1;
  }
  let close = at + scheme[0].length;
  while (close < text.length && !AUTOLINK_STOP.test(text.charAt(close))) {
    scan.context.work.count += 1;
    close += 1;
  }
  if (text.charAt(close) !== ">") {
    return -1;
  }
  const url = text.slice(at + 1, close);
  scan.flush();
  scan.out.text(url, isSafeUrl(url) ? { ...scan.marks, href: url } : scan.marks);
  return close + 1;
}

/** Emphasis, strong or strikethrough; an opener that closes nothing is literal text. */
function emphasis(scan: InlineScan, at: number): number {
  const end = runEnd(scan.text, at);
  if (scan.depth < MAX_NESTING && opens(scan.text, at, end)) {
    const taken = scan.text.charAt(at) === "~" ? strike(scan, at, end) : strongOrEm(scan, at, end);
    if (taken >= 0) {
      return taken;
    }
  }
  scan.plain += scan.text.slice(at, end);
  return end;
}

/** Whether a delimiter run can open: text after it, and an underscore run no letter before it. */
function opens(text: string, at: number, end: number): boolean {
  const after = text.charAt(end);
  if (after === "" || WHITESPACE.test(after)) {
    return false;
  }
  return text.charAt(at) !== "_" || !WORD.test(text.charAt(at - 1));
}

function strike(scan: InlineScan, at: number, end: number): number {
  const close = end - at === 2 ? scan.closer("~", end, 2) : null;
  if (close === null) {
    return -1;
  }
  scan.flush();
  const marks: Marks = { ...scan.marks, strike: true };
  inline(scan.context, scan.text.slice(end, close), marks, scan.depth + 1, scan.out);
  return close + 2;
}

/** `*`/`_` emphasis: the longest opener that closes, its extra delimiters left as text. */
function strongOrEm(scan: InlineScan, at: number, end: number): number {
  const run = end - at;
  if (run > 3) {
    return -1;
  }
  const ch = scan.text.charAt(at);
  for (let length = run; length >= 1; length -= 1) {
    const close = scan.closer(ch, end, length);
    if (close !== null) {
      scan.flush();
      scan.out.text(scan.text.slice(at, end - length), scan.marks);
      const marks = emphasisMarks(scan.marks, length);
      inline(scan.context, scan.text.slice(end, close), marks, scan.depth + 1, scan.out);
      return close + length;
    }
  }
  return -1;
}

function emphasisMarks(marks: Marks, length: number): Marks {
  if (length === 3) {
    return { ...marks, strong: true, em: true };
  }
  return length === 2 ? { ...marks, strong: true } : { ...marks, em: true };
}

/** A link: its text with an `href` when its URL is safe, else its text alone. */
function link(scan: InlineScan, at: number): number {
  const target = scan.depth < MAX_NESTING ? linkTarget(scan, at) : null;
  if (target === null) {
    return -1;
  }
  scan.flush();
  const marks = isSafeUrl(target.url) ? { ...scan.marks, href: target.url.trim() } : scan.marks;
  inline(scan.context, scan.text.slice(at + 1, target.close), marks, scan.depth + 1, scan.out);
  return target.end;
}

/** An image: its alt text, never the image. */
function image(scan: InlineScan, at: number): number {
  const opener = scan.text.charAt(at + 1) === "[";
  const target = opener && scan.depth < MAX_NESTING ? linkTarget(scan, at + 1) : null;
  if (target === null) {
    return -1;
  }
  scan.flush();
  const alt = new SpanList();
  inline(scan.context, scan.text.slice(at + 2, target.close), NO_MARKS, scan.depth + 1, alt);
  scan.out.text(alt.spans.map((span) => (span.br === true ? " " : span.text)).join(""), scan.marks);
  return target.end;
}

interface LinkTarget {
  /** Where the link text's `]` is. */
  readonly close: number;
  readonly url: string;
  /** Just after the destination's `)`. */
  readonly end: number;
}

/** The `](destination)` after the `[` at `open`; null when there is none. */
function linkTarget(scan: InlineScan, open: number): LinkTarget | null {
  const close = scan.brackets.get(open);
  if (close === undefined || scan.text.charAt(close + 1) !== "(") {
    return null;
  }
  const target = destination(scan, close + 2);
  return target === null ? null : { close, ...target };
}

/** Where a destination's URL lies in the text, and where the text after it starts. */
interface UrlRange {
  readonly start: number;
  readonly end: number;
  readonly next: number;
}

/** A link's `destination "title")` from just after its `(`; null when it does not close. */
function destination(scan: InlineScan, from: number): { url: string; end: number } | null {
  const start = skipSpace(scan.text, from);
  let range: UrlRange | null;
  if (scan.text.charAt(start) === "<") {
    range = angleUrl(scan, start);
  } else if (start === from) {
    range = parenUrl(scan, from);
  } else {
    range = scanUrl(scan, start);
  }
  return range === null ? null : closeDestination(scan, range);
}

/** A `<…>` URL: no line break or `<` inside, within the length limit. */
function angleUrl(scan: InlineScan, start: number): UrlRange | null {
  const close = scan.nextIndex(">", start + 1);
  const fits =
    close < scan.text.length &&
    close - start <= URL_LIMIT &&
    scan.nextIndex("\n", start + 1) > close &&
    scan.nextIndex("<", start + 1) > close;
  return fits ? { start: start + 1, end: close, next: close + 1 } : null;
}

/** A URL right after the `(`: it runs to the `)` that balances the `(`, or to whitespace. */
function parenUrl(scan: InlineScan, from: number): UrlRange | null {
  const run = scan.parens.get(from - 1);
  if (run === undefined || run.end - from > URL_LIMIT) {
    return null;
  }
  return { start: from, end: run.end, next: run.end };
}

/** A URL after space inside the `(`: scanned, at most the length limit, to its balancing `)` or whitespace. */
function scanUrl(scan: InlineScan, start: number): UrlRange | null {
  const { text } = scan;
  const limit = Math.min(text.length, start + URL_LIMIT);
  let at = start;
  let parens = 0;
  while (at < limit && !endsUrl(text.charAt(at), parens)) {
    scan.context.work.count += 1;
    parens += parenDelta(text.charAt(at));
    at += text.charAt(at) === "\\" ? 2 : 1;
  }
  const end = Math.min(at, text.length);
  return at >= limit && limit < text.length ? null : { start, end, next: end };
}

function endsUrl(ch: string, parens: number): boolean {
  return WHITESPACE.test(ch) || (ch === ")" && parens === 0);
}

function parenDelta(ch: string): number {
  if (ch === "(") {
    return 1;
  }
  return ch === ")" ? -1 : 0;
}

/** After the URL: an optional title, then the `)`. */
function closeDestination(scan: InlineScan, range: UrlRange): { url: string; end: number } | null {
  const { text } = scan;
  let at = skipSpace(text, range.next);
  const quote = text.charAt(at);
  if (quote === '"' || quote === "'" || quote === "(") {
    const close = scan.nextIndex(quote === "(" ? ")" : quote, at + 1);
    if (close >= text.length) {
      return null;
    }
    at = skipSpace(text, close + 1);
  }
  if (text.charAt(at) !== ")") {
    return null;
  }
  const url = text.slice(range.start, range.end).replaceAll(ESCAPED, "$1");
  return { url, end: at + 1 };
}

/** Past the whitespace at `from`, at most the length limit of it. */
function skipSpace(text: string, from: number): number {
  let at = from;
  while (at - from < URL_LIMIT && WHITESPACE.test(text.charAt(at))) {
    at += 1;
  }
  return at;
}

const HANDLERS: ReadonlyMap<string, Handler> = new Map<string, Handler>([
  ["\\", backslash],
  ["\n", newline],
  ["`", backtick],
  ["<", autolink],
  ["*", emphasis],
  ["_", emphasis],
  ["~", emphasis],
  ["[", link],
  ["!", image],
]);

/** Whether a link's URL, trimmed, is an `http://` or `https://` one with no space or control character. */
function isSafeUrl(url: string): boolean {
  const trimmed = url.trim();
  if (!SAFE_SCHEME.test(trimmed)) {
    return false;
  }
  for (const ch of trimmed) {
    const code = ch.codePointAt(0) ?? 0;
    if (code <= 0x20 || code === 0x7f) {
      return false;
    }
  }
  return true;
}
