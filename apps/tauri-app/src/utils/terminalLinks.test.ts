import { describe, expect, it } from "vitest";
import {
  canStitch,
  detectTerminalLinks,
  detectTerminalRowLinks,
  type TerminalRow,
} from "./terminalLinks";

function row(text: string, cols: number, isWrapped = false): TerminalRow {
  return { text: text.padEnd(cols, " "), isWrapped };
}

describe("detectTerminalLinks", () => {
  it("detects a URL and drops trailing punctuation", () => {
    const links = detectTerminalLinks("see https://example.com/a, ok");
    expect(links).toHaveLength(1);
    expect(links[0]?.kind).toBe("url");
    expect(links[0]?.target).toBe("https://example.com/a");
    expect(links[0]?.candidates).toEqual([
      { path: "https://example.com/a", line: null, column: null },
    ]);
    expect(links[0]?.startRow).toBe(0);
    expect(links[0]?.endRow).toBe(0);
    expect(links[0]?.startColumn).toBe(links[0]?.startIndex);
    expect(links[0]?.endColumn).toBe(links[0]?.endIndex);
  });

  it("detects a path with a line and column reference", () => {
    const links = detectTerminalLinks("at src/main.rs:42:7 failed");
    expect(links).toHaveLength(1);
    expect(links[0]?.kind).toBe("path");
    expect(links[0]?.target).toBe("src/main.rs");
    expect(links[0]?.line).toBe(42);
    expect(links[0]?.column).toBe(7);
    expect(links[0]?.candidates).toEqual([
      { path: "src/main.rs", line: 42, column: 7 },
    ]);
  });

  it("detects a parenthesised path without its trailing punctuation", () => {
    const links = detectTerminalLinks("(see ./docs/plan.md).");
    expect(links).toHaveLength(1);
    expect(links[0]?.target).toBe("./docs/plan.md");
    expect(links[0]?.line).toBeNull();
    expect(links[0]?.column).toBeNull();
  });
});

describe("detectTerminalRowLinks", () => {
  it("merges a path across a soft wrap", () => {
    const cols = 20;
    const rows = [
      row("See X:/dev/project/l", cols),
      row("ong/name/file.ts:12", cols, true),
    ];

    const links = detectTerminalRowLinks(rows, cols);

    expect(links).toHaveLength(1);
    expect(links[0]?.target).toBe("X:/dev/project/long/name/file.ts");
    expect(links[0]?.line).toBe(12);
    expect(links[0]?.candidates).toHaveLength(1);
    expect(links[0]?.startRow).toBe(0);
    expect(links[0]?.startColumn).toBe(4);
    expect(links[0]?.endRow).toBe(1);
    expect(links[0]?.endColumn).toBe(19);
  });

  it("merges a path across a box's hard wrap and keeps the fragment as a fallback", () => {
    const cols = 28;
    const rows = [
      row("\u2502 X:/dev/proj/notes-file \u2502", cols),
      row("\u2502 le.txt:7:3             \u2502", cols),
    ];

    expect(canStitch(rows[0]!, rows[1]!, cols)).toBe(true);

    const links = detectTerminalRowLinks(rows, cols);

    expect(links).toHaveLength(1);
    expect(links[0]?.target).toBe("X:/dev/proj/notes-filele.txt");
    expect(links[0]?.candidates).toEqual([
      { path: "X:/dev/proj/notes-filele.txt", line: 7, column: 3 },
      { path: "X:/dev/proj/notes-file", line: null, column: null },
    ]);
    expect(links[0]?.startRow).toBe(0);
    expect(links[0]?.startColumn).toBe(2);
    expect(links[0]?.endRow).toBe(1);
    expect(links[0]?.endColumn).toBe(12);
  });

  it("rejects a continuation when the row above stops short of the wrap column", () => {
    const cols = 40;
    const rows = [row("Wrote to ./output.txt", cols), row("done", cols)];

    expect(canStitch(rows[0]!, rows[1]!, cols)).toBe(false);

    const links = detectTerminalRowLinks(rows, cols);

    expect(links).toHaveLength(1);
    expect(links[0]?.target).toBe("./output.txt");
    expect(links[0]?.candidates).toHaveLength(1);
  });

  it("rejects a continuation longer than the row it would continue", () => {
    const cols = 24;
    const rows = [
      row("\u2502 a/b.ts \u2502", cols),
      row("\u2502 continuation-here \u2502", cols),
    ];

    expect(canStitch(rows[0]!, rows[1]!, cols)).toBe(false);

    const links = detectTerminalRowLinks(rows, cols);

    expect(links.map((link) => link.target)).toEqual(["a/b.ts"]);
  });

  it("rejects a continuation that starts with whitespace", () => {
    const cols = 20;
    const rows = [row("Log at /var/log/a.tx", cols), row("    t-file.log", cols)];

    expect(canStitch(rows[0]!, rows[1]!, cols)).toBe(false);

    const links = detectTerminalRowLinks(rows, cols);

    expect(links).toHaveLength(1);
    expect(links[0]?.target).toBe("/var/log/a.tx");
    expect(links[0]?.candidates).toHaveLength(1);
  });
});
