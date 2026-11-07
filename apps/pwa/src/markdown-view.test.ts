import { expect, test } from "vitest";

import { parseMarkdown } from "./markdown";
import { renderMarkdown } from "./markdown-view";

/** The few node calls the Markdown view makes; a text node's tag is `#text`. */
class FakeNode {
  className = "";
  textContent = "";
  readonly style = { textAlign: "" };
  readonly children: FakeNode[] = [];

  constructor(readonly tagName: string) {}

  append(...nodes: FakeNode[]): void {
    this.children.push(...nodes);
  }

  prepend(...nodes: FakeNode[]): void {
    this.children.unshift(...nodes);
  }

  /** Every descendant, depth first, in drawing order. */
  descendants(): FakeNode[] {
    const found: FakeNode[] = [];
    for (const child of this.children) {
      found.push(child, ...child.descendants());
    }
    return found;
  }

  /** The text a reader sees: a text node's own, else its children's. */
  text(): string {
    if (this.tagName === "#text") {
      return this.textContent;
    }
    return this.children.length === 0
      ? this.textContent
      : this.children.map((child) => child.text()).join("");
  }

  /** Every property set on the node apart from its tag, class, text, style and children. */
  props(): Record<string, unknown> {
    const { className, textContent, style, children, tagName, ...rest } = this;
    void [className, textContent, style, children, tagName];
    return rest;
  }
}

class FakeDocument {
  createElement(tag: string): FakeNode {
    return new FakeNode(tag.toLowerCase());
  }

  createTextNode(text: string): FakeNode {
    const node = new FakeNode("#text");
    node.textContent = text;
    return node;
  }
}

function render(text: string): FakeNode {
  const doc = new FakeDocument();
  return renderMarkdown(doc as unknown as Document, parseMarkdown(text)) as unknown as FakeNode;
}

function tags(root: FakeNode): string[] {
  return root.descendants().map((node) => node.tagName);
}

function only(root: FakeNode, tag: string): FakeNode {
  const found = root.descendants().filter((node) => node.tagName === tag);
  expect(found, tag).toHaveLength(1);
  const [node] = found;
  if (node === undefined) {
    throw new Error(`no ${tag}`);
  }
  return node;
}

test("the view is a div.md holding the blocks", () => {
  const root = render("one\n\ntwo");
  expect(root.tagName).toBe("div");
  expect(root.className).toBe("md");
  expect(root.children.map((node) => node.tagName)).toEqual(["p", "p"]);
});

test("a heading is its level's element", () => {
  const root = render("## Title *x*");
  const heading = only(root, "h2");
  expect(heading.text()).toBe("Title x");
  expect(tags(heading)).toEqual(["#text", "em", "#text"]);
});

test("a paragraph's line break is a br", () => {
  const p = only(render("a\nb"), "p");
  expect(p.children.map((node) => node.tagName)).toEqual(["#text", "br", "#text"]);
  expect(p.text()).toBe("ab");
});

test("a code block's text is in its code element's textContent", () => {
  const root = render("```html\n<b>x</b>\n```");
  const pre = only(root, "pre");
  const code = only(pre, "code");
  expect(code.textContent).toBe("<b>x</b>");
  expect(code.children).toEqual([]);
  expect(tags(root)).not.toContain("b");
});

test("a table scrolls in its box and aligns its cells", () => {
  const root = render("| a | b | c |\n|:--|:-:|---|\n| 1 | 2 | 3 |");
  const box = only(root, "div");
  expect(box.className).toBe("md-table");
  expect(tags(box)).toEqual([
    "table",
    "thead",
    "tr",
    "th",
    "#text",
    "th",
    "#text",
    "th",
    "#text",
    "tbody",
    "tr",
    "td",
    "#text",
    "td",
    "#text",
    "td",
    "#text",
  ]);
  const cells = box.descendants().filter((node) => node.tagName === "td");
  expect(cells.map((cell) => cell.style.textAlign)).toEqual(["left", "center", ""]);
  expect(cells.map((cell) => cell.text())).toEqual(["1", "2", "3"]);
});

test("a safe link is an anchor that opens apart", () => {
  const a = only(render("see [x](https://e.com/a)"), "a");
  expect(a.props()).toEqual({
    href: "https://e.com/a",
    rel: "noopener noreferrer",
    target: "_blank",
  });
  expect(a.text()).toBe("x");
});

test("a list keeps its start, and a task item's box is a disabled checkbox", () => {
  const root = render("3. a\n4. b\n\n- [x] done\n- [ ] todo");
  const ol = only(root, "ol");
  expect(ol.props()).toEqual({ start: 3 });
  const boxes = root.descendants().filter((node) => node.tagName === "input");
  expect(boxes.map((box) => box.props())).toEqual([
    { type: "checkbox", disabled: true, checked: true },
    { type: "checkbox", disabled: true, checked: false },
  ]);
  expect(tags(root)).toContain("ul");
});

test("hostile text makes no element, link or attribute of its own", () => {
  const root = render(
    [
      "<script>alert(1)</script>",
      "<img src=x onerror=alert(1)>",
      "[x](javascript:alert(1)) ![i](https://evil.example/x.png)",
      "| <b onclick=alert(1)> |",
      "|:-:|",
    ].join("\n\n"),
  );
  const found = tags(root);
  expect(found).not.toContain("script");
  expect(found).not.toContain("img");
  expect(found).not.toContain("a");
  expect(found).not.toContain("b");
  expect(root.text()).toContain("<script>alert(1)</script>");
  expect(root.text()).toContain("<img src=x onerror=alert(1)>");
  for (const node of [root, ...root.descendants()]) {
    expect(Object.values(node.props()).filter((value) => typeof value === "string")).toEqual([]);
    expect(["", "left", "center", "right"]).toContain(node.style.textAlign);
    expect(node.className).not.toContain("alert");
  }
});
