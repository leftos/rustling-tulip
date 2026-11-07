import type { MdAlign, MdBlock, MdListItem, MdSpan } from "./markdown";

/**
 * Markdown blocks as DOM nodes, for an answer row of the session screen.
 * Agent text is untrusted: every agent string reaches the DOM as a text node
 * or `textContent`, never as HTML, and the only attribute it sets is a safe
 * link's `href` (the parser keeps only `http:` and `https:` ones). A table
 * cell's alignment comes from the parser's fixed set of values.
 */

const HEADINGS = ["h1", "h2", "h3", "h4", "h5", "h6"] as const;

/** The blocks inside a `div.md`. */
export function renderMarkdown(doc: Document, blocks: readonly MdBlock[]): HTMLDivElement {
  const root = doc.createElement("div");
  root.className = "md";
  root.append(...blocks.map((block) => blockNode(doc, block)));
  return root;
}

function blockNode(doc: Document, block: MdBlock): HTMLElement {
  switch (block.kind) {
    case "heading": {
      const heading = doc.createElement(HEADINGS[block.level - 1] ?? "h6");
      heading.append(...spanNodes(doc, block.spans));
      return heading;
    }
    case "paragraph": {
      const p = doc.createElement("p");
      p.append(...spanNodes(doc, block.spans));
      return p;
    }
    case "code": {
      const pre = doc.createElement("pre");
      const code = doc.createElement("code");
      code.textContent = block.text;
      pre.append(code);
      return pre;
    }
    case "quote": {
      const quote = doc.createElement("blockquote");
      quote.append(...block.blocks.map((inner) => blockNode(doc, inner)));
      return quote;
    }
    case "list":
      return listNode(doc, block.ordered, block.start, block.items);
    case "table":
      return tableNode(doc, block.align, block.header, block.rows);
    case "rule":
      return doc.createElement("hr");
  }
}

function listNode(
  doc: Document,
  ordered: boolean,
  start: number,
  items: readonly MdListItem[],
): HTMLElement {
  const list = ordered ? doc.createElement("ol") : doc.createElement("ul");
  if (ordered && start !== 1) {
    (list as HTMLOListElement).start = start;
  }
  for (const item of items) {
    const li = doc.createElement("li");
    const blocks = item.blocks.map((block) => blockNode(doc, block));
    if (item.task !== null) {
      const box = doc.createElement("input");
      box.type = "checkbox";
      box.disabled = true;
      box.checked = item.task;
      li.className = "md-task";
      // The box leads the item's first paragraph, so it sits on the text's line.
      const [first] = blocks;
      if (first !== undefined && first.tagName.toLowerCase() === "p") {
        first.prepend(box);
      } else {
        blocks.unshift(box);
      }
    }
    li.append(...blocks);
    list.append(li);
  }
  return list;
}

function tableNode(
  doc: Document,
  align: readonly MdAlign[],
  header: readonly (readonly MdSpan[])[],
  rows: readonly (readonly (readonly MdSpan[])[])[],
): HTMLElement {
  const box = doc.createElement("div");
  box.className = "md-table";
  const table = doc.createElement("table");
  const head = doc.createElement("thead");
  head.append(rowNode(doc, "th", align, header));
  table.append(head);
  if (rows.length > 0) {
    const body = doc.createElement("tbody");
    body.append(...rows.map((row) => rowNode(doc, "td", align, row)));
    table.append(body);
  }
  box.append(table);
  return box;
}

function rowNode(
  doc: Document,
  tag: "th" | "td",
  align: readonly MdAlign[],
  cells: readonly (readonly MdSpan[])[],
): HTMLTableRowElement {
  const tr = doc.createElement("tr");
  cells.forEach((spans, at) => {
    const cell = doc.createElement(tag);
    const side = align[at] ?? null;
    if (side !== null) {
      cell.style.textAlign = side;
    }
    cell.append(...spanNodes(doc, spans));
    tr.append(cell);
  });
  return tr;
}

function spanNodes(doc: Document, spans: readonly MdSpan[]): Node[] {
  return spans.map((span) => spanNode(doc, span));
}

/** One span: its text node, wrapped in an element per mark, the link outermost. */
function spanNode(doc: Document, span: MdSpan): Node {
  if (span.br === true) {
    return doc.createElement("br");
  }
  let node: Node = doc.createTextNode(span.text);
  const wrap = (tag: "code" | "s" | "em" | "strong"): void => {
    const outer = doc.createElement(tag);
    outer.append(node);
    node = outer;
  };
  if (span.code === true) {
    wrap("code");
  }
  if (span.strike === true) {
    wrap("s");
  }
  if (span.em === true) {
    wrap("em");
  }
  if (span.strong === true) {
    wrap("strong");
  }
  if (span.href !== undefined) {
    const a = doc.createElement("a");
    a.href = span.href;
    a.rel = "noopener noreferrer";
    a.target = "_blank";
    a.append(node);
    node = a;
  }
  return node;
}
