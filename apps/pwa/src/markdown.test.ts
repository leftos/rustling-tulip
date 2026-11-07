import { expect, test } from "vitest";

import { parseMarkdown, type MdBlock, type MdSpan, type ParseWork } from "./markdown";

/** A paragraph of one plain span. */
function para(text: string): MdBlock {
  return { kind: "paragraph", spans: [{ text }] };
}

/** The spans of a text that parses to one paragraph. */
function spans(text: string): readonly MdSpan[] {
  const blocks = parseMarkdown(text);
  expect(blocks).toHaveLength(1);
  const [block] = blocks;
  if (block?.kind !== "paragraph") {
    throw new Error(`expected a paragraph, got ${JSON.stringify(block)}`);
  }
  return block.spans;
}

test("a heading keeps its level and its spans", () => {
  expect(parseMarkdown("# One")).toEqual([{ kind: "heading", level: 1, spans: [{ text: "One" }] }]);
  expect(parseMarkdown("## Two **b** ##")).toEqual([
    { kind: "heading", level: 2, spans: [{ text: "Two " }, { text: "b", strong: true }] },
  ]);
  expect(parseMarkdown("###### Six")).toEqual([
    { kind: "heading", level: 6, spans: [{ text: "Six" }] },
  ]);
});

test("a setext underline makes the paragraph above it a heading", () => {
  expect(parseMarkdown("Title\n===\n\nSub\n---")).toEqual([
    { kind: "heading", level: 1, spans: [{ text: "Title" }] },
    { kind: "heading", level: 2, spans: [{ text: "Sub" }] },
  ]);
});

test("paragraphs are split by blank lines", () => {
  expect(parseMarkdown("one\n\ntwo")).toEqual([para("one"), para("two")]);
});

test("a fenced code block keeps its text and the info string's first word", () => {
  expect(parseMarkdown("```ts title\nconst a = 1;\n  b();\n```\nafter")).toEqual([
    { kind: "code", lang: "ts", text: "const a = 1;\n  b();" },
    para("after"),
  ]);
  expect(parseMarkdown("~~~\nx\n~~~")).toEqual([{ kind: "code", lang: "", text: "x" }]);
});

test("an indented code block drops four spaces of indent", () => {
  expect(parseMarkdown("    let x;\n\n      y\n")).toEqual([
    { kind: "code", lang: "", text: "let x;\n\n  y" },
  ]);
});

test("a block quote holds blocks", () => {
  expect(parseMarkdown("> a\n> b")).toEqual([
    {
      kind: "quote",
      blocks: [
        { kind: "paragraph", spans: [{ text: "a" }, { text: "", br: true }, { text: "b" }] },
      ],
    },
  ]);
});

test("a bullet list and an ordered list with its start", () => {
  expect(parseMarkdown("- a\n- b")).toEqual([
    {
      kind: "list",
      ordered: false,
      start: 1,
      items: [
        { task: null, blocks: [para("a")] },
        { task: null, blocks: [para("b")] },
      ],
    },
  ]);
  expect(parseMarkdown("3. a\n4. b")).toEqual([
    {
      kind: "list",
      ordered: true,
      start: 3,
      items: [
        { task: null, blocks: [para("a")] },
        { task: null, blocks: [para("b")] },
      ],
    },
  ]);
});

test("a table has its header and rows", () => {
  expect(parseMarkdown("| a | b |\n|---|---|\n| 1 | **2** |")).toEqual([
    {
      kind: "table",
      align: [null, null],
      header: [[{ text: "a" }], [{ text: "b" }]],
      rows: [[[{ text: "1" }], [{ text: "2", strong: true }]]],
    },
  ]);
});

test("a thematic break is a rule", () => {
  expect(parseMarkdown("a\n\n---\n\n***\n\n_ _ _")).toEqual([
    para("a"),
    { kind: "rule" },
    { kind: "rule" },
    { kind: "rule" },
  ]);
});

test("a nested list sits inside its parent item", () => {
  expect(parseMarkdown("- a\n  - b\n- c")).toEqual([
    {
      kind: "list",
      ordered: false,
      start: 1,
      items: [
        {
          task: null,
          blocks: [
            para("a"),
            {
              kind: "list",
              ordered: false,
              start: 1,
              items: [{ task: null, blocks: [para("b")] }],
            },
          ],
        },
        { task: null, blocks: [para("c")] },
      ],
    },
  ]);
});

test("a nested quote and a list inside a quote", () => {
  expect(parseMarkdown("> a\n> > b\n>\n> - c")).toEqual([
    {
      kind: "quote",
      blocks: [
        para("a"),
        { kind: "quote", blocks: [para("b")] },
        {
          kind: "list",
          ordered: false,
          start: 1,
          items: [{ task: null, blocks: [para("c")] }],
        },
      ],
    },
  ]);
});

test("task items are checked and unchecked", () => {
  expect(parseMarkdown("- [x] done\n- [ ] todo\n- [X] also")).toEqual([
    {
      kind: "list",
      ordered: false,
      start: 1,
      items: [
        { task: true, blocks: [para("done")] },
        { task: false, blocks: [para("todo")] },
        { task: true, blocks: [para("also")] },
      ],
    },
  ]);
});

test("a table keeps each column's alignment and pads a short row", () => {
  expect(parseMarkdown("| a | b | c | d |\n|:--|:-:|--:|---|\n| 1 | 2 |")).toEqual([
    {
      kind: "table",
      align: ["left", "center", "right", null],
      header: [[{ text: "a" }], [{ text: "b" }], [{ text: "c" }], [{ text: "d" }]],
      rows: [[[{ text: "1" }], [{ text: "2" }], [], []]],
    },
  ]);
});

test("a single line break is a br span", () => {
  expect(spans("a\nb")).toEqual([{ text: "a" }, { text: "", br: true }, { text: "b" }]);
});

test("strong, em, strike and code spans", () => {
  expect(spans("**s**")).toEqual([{ text: "s", strong: true }]);
  expect(spans("__s__")).toEqual([{ text: "s", strong: true }]);
  expect(spans("*e*")).toEqual([{ text: "e", em: true }]);
  expect(spans("_e_")).toEqual([{ text: "e", em: true }]);
  expect(spans("~~x~~")).toEqual([{ text: "x", strike: true }]);
  expect(spans("`c`")).toEqual([{ text: "c", code: true }]);
  expect(spans("a `x` b")).toEqual([{ text: "a " }, { text: "x", code: true }, { text: " b" }]);
});

test("marks mix and nest", () => {
  expect(spans("**a *b* c**")).toEqual([
    { text: "a ", strong: true },
    { text: "b", strong: true, em: true },
    { text: " c", strong: true },
  ]);
  expect(spans("***x***")).toEqual([{ text: "x", strong: true, em: true }]);
  expect(spans("**bold *em***")).toEqual([
    { text: "bold ", strong: true },
    { text: "em", strong: true, em: true },
  ]);
  expect(spans("~~a **b**~~")).toEqual([
    { text: "a ", strike: true },
    { text: "b", strike: true, strong: true },
  ]);
  expect(spans("`**x**`")).toEqual([{ text: "**x**", code: true }]);
  expect(spans("snake_case_name and 2 * 3 * 4")).toEqual([
    { text: "snake_case_name and 2 * 3 * 4" },
  ]);
});

test("an http or https link carries its href", () => {
  expect(spans("[x](https://e.com)")).toEqual([{ text: "x", href: "https://e.com" }]);
  expect(spans("see [**x**](http://e.com/a?b=1)")).toEqual([
    { text: "see " },
    { text: "x", strong: true, href: "http://e.com/a?b=1" },
  ]);
  expect(spans("[x]( HTTPS://e.com )")).toEqual([{ text: "x", href: "HTTPS://e.com" }]);
  expect(spans("<https://e.com>")).toEqual([{ text: "https://e.com", href: "https://e.com" }]);
});

test("any other link is its text and carries no href", () => {
  for (const text of [
    "[x](javascript:alert(1))",
    "[x](JavaScript:alert(1))",
    "[x]( javascript:alert(1))",
    "[x](data:text/html,alert(1))",
    "[x](/relative)",
    "[x](https:)",
  ]) {
    expect(spans(text), text).toEqual([{ text: "x" }]);
  }
  expect(spans("<javascript:alert(1)>")).toEqual([{ text: "javascript:alert(1)" }]);
});

test("an image is its alt text", () => {
  expect(spans("![alt](https://e.com/a.png)")).toEqual([{ text: "alt" }]);
  expect(spans("a ![alt](https://e.com/a.png) b")).toEqual([{ text: "a alt b" }]);
});

test("raw HTML stays literal text", () => {
  expect(spans("<script>alert(1)</script>")).toEqual([{ text: "<script>alert(1)</script>" }]);
  expect(spans("<img src=x onerror=alert(1)>")).toEqual([{ text: "<img src=x onerror=alert(1)>" }]);
});

test("backslash escapes keep the character and entities stay as written", () => {
  expect(spans("\\*not em\\* \\[x\\](y)")).toEqual([{ text: "*not em* [x](y)" }]);
  expect(spans("&amp; &lt;b&gt;")).toEqual([{ text: "&amp; &lt;b&gt;" }]);
});

test("an unclosed fence runs to the end of the text", () => {
  expect(parseMarkdown("intro\n\n```py\nprint(1)\n\nmore")).toEqual([
    para("intro"),
    { kind: "code", lang: "py", text: "print(1)\n\nmore" },
  ]);
});

test("empty text has no blocks", () => {
  expect(parseMarkdown("")).toEqual([]);
  expect(parseMarkdown("  \n\n")).toEqual([]);
});

test("text with no Markdown is one paragraph equal to the input", () => {
  expect(parseMarkdown("Added reset_for_target.")).toEqual([para("Added reset_for_target.")]);
});

test("deep nesting is capped and does not throw", () => {
  const blocks = parseMarkdown(`${">".repeat(5000)} x\n${"- ".repeat(5000)}y`);
  expect(blocks.length).toBeGreaterThan(0);
});

test("emphasis openers closed by longer runs parse in linear work", () => {
  const text = "*a** ".repeat(20000);
  const work: ParseWork = { count: 0 };
  const [block] = parseMarkdown(text, work);
  if (block?.kind !== "paragraph") {
    throw new Error("expected a paragraph");
  }
  expect(block.spans.slice(0, 3)).toEqual([
    { text: "a*", em: true },
    { text: " " },
    { text: "a*", em: true },
  ]);
  expect(block.spans).toHaveLength(39999);
  expect(work.count).toBeGreaterThan(0);
  expect(work.count).toBeLessThan(20 * text.length);
});

test("unclosed link destinations parse in linear work", () => {
  const text = "[a](".repeat(20000);
  const work: ParseWork = { count: 0 };
  expect(parseMarkdown(text, work)).toEqual([para(text)]);
  expect(work.count).toBeGreaterThan(0);
  expect(work.count).toBeLessThan(20 * text.length);
});

test("a heading with a long run of inner spaces keeps them", () => {
  const gap = " ".repeat(20000);
  expect(parseMarkdown(`# a${gap}b`)).toEqual([
    { kind: "heading", level: 1, spans: [{ text: `a${gap}b` }] },
  ]);
});

test("a backslash inside a code span is literal", () => {
  expect(spans("Run `X:\\dev\\` now")).toEqual([
    { text: "Run " },
    { text: "X:\\dev\\", code: true },
    { text: " now" },
  ]);
});

test("a one-column header without a pipe in its delimiter row is not a table", () => {
  const blocks = parseMarkdown("a | \n---");
  expect(blocks.map((block) => block.kind)).not.toContain("table");
  expect(blocks).toEqual([{ kind: "heading", level: 2, spans: [{ text: "a |" }] }]);
});
