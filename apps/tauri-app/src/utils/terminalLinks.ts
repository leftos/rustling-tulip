export type TerminalLinkKind = "url" | "path";

export interface DetectedTerminalLink {
  kind: TerminalLinkKind;
  text: string;
  target: string;
  startIndex: number;
  endIndex: number;
  line: number | null;
  column: number | null;
}

const URL_PATTERN = /\bhttps?:\/\/[^\s<>"'`{}|\\^]+/gi;
const PATH_PATTERN =
  /(^|[\s([{<])((?:[A-Za-z]:[\\/]|\\\\[^\s\\/:*?"<>|]+[\\/][^\s\\/:*?"<>|]+[\\/]|\/|\.{1,2}[\\/]|[A-Za-z0-9_.-]+[\\/])[^\s<>"'`|]*)/g;
const TRAILING_PUNCTUATION = /[),.;!?}\]]+$/;
const LINE_COLUMN_SUFFIX = /^(.*?)(?::([1-9]\d{0,6})(?::([1-9]\d{0,6}))?)$/;

export function detectTerminalLinks(text: string): DetectedTerminalLink[] {
  const links: DetectedTerminalLink[] = [];
  for (const match of text.matchAll(URL_PATTERN)) {
    const raw = match[0];
    const startIndex = match.index ?? 0;
    const trimmed = trimLinkCandidate(raw);
    if (!trimmed) continue;
    links.push({
      kind: "url",
      text: trimmed,
      target: trimmed,
      startIndex,
      endIndex: startIndex + trimmed.length,
      line: null,
      column: null,
    });
  }

  for (const match of text.matchAll(PATH_PATTERN)) {
    const raw = match[2] ?? "";
    const prefix = match[1] ?? "";
    const startIndex = (match.index ?? 0) + prefix.length;
    const trimmed = trimLinkCandidate(raw);
    if (!trimmed || overlapsExistingLink(startIndex, startIndex + trimmed.length, links)) {
      continue;
    }
    const path = splitPathPosition(trimmed);
    links.push({
      kind: "path",
      text: trimmed,
      target: path.path,
      startIndex,
      endIndex: startIndex + trimmed.length,
      line: path.line,
      column: path.column,
    });
  }

  return links.sort((a, b) => a.startIndex - b.startIndex);
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

function splitPathPosition(value: string): {
  path: string;
  line: number | null;
  column: number | null;
} {
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
