import { expect, test } from "vitest";

import { planRows, syncChildren, type ChildList, type DrawnRow } from "./row-reuse";

interface Row {
  readonly id: string;
  readonly text: string;
}

interface Node {
  readonly label: string;
}

function row(id: string, text: string): Row {
  return { id, text };
}

/** Whether two rows draw the same thing: the same text under the same id. */
function sameText(left: Row, right: Row): boolean {
  return left.id === right.id && left.text === right.text;
}

/** A row builder that records the text of every row it built. */
function countingBuild(): { build: (source: Row) => Node; built: string[] } {
  const built: string[] = [];
  return {
    built,
    build: (source) => {
      built.push(source.text);
      return { label: source.text };
    },
  };
}

function drawnMap(entries: readonly DrawnRow<Row, Node>[]): Map<string, DrawnRow<Row, Node>> {
  return new Map(entries.map((entry) => [entry.row.id, entry]));
}

/** A child list over an array that records every call made to it. */
class FakeChildren implements ChildList<Node> {
  children: Node[];
  readonly calls: string[] = [];

  constructor(children: readonly Node[]) {
    this.children = [...children];
  }

  insertBefore(node: Node, ref: Node | null): void {
    this.calls.push(`insertBefore ${node.label} before ${ref?.label ?? "null"}`);
    const at = ref === null ? this.children.length : this.children.indexOf(ref);
    this.children.splice(at, 0, node);
  }

  removeChild(node: Node): void {
    this.calls.push(`removeChild ${node.label}`);
    this.children.splice(this.children.indexOf(node), 1);
  }
}

test("planRows reuses the node of an unchanged row and builds a changed or new one", () => {
  const a = row("a", "one");
  const nodeA: Node = { label: "a" };
  const nodeB: Node = { label: "b" };
  const drawn = drawnMap([
    { row: a, node: nodeA },
    { row: row("b", "two"), node: nodeB },
  ]);
  const { build, built } = countingBuild();

  const plan = planRows(drawn, [a, row("b", "changed"), row("c", "three")], sameText, build);

  expect(plan[0]?.node).toBe(nodeA);
  expect(plan[1]?.node).not.toBe(nodeB);
  expect(plan.map((entry) => entry.row.id)).toEqual(["a", "b", "c"]);
  expect(plan.map((entry) => entry.node)).toEqual([
    nodeA,
    { label: "changed" },
    { label: "three" },
  ]);
  expect(built).toEqual(["changed", "three"]);
});

test("planRows rebuilds a row whose id moved to another value", () => {
  const node: Node = { label: "old" };
  const drawn = drawnMap([{ row: row("a", "one"), node }]);
  const { build, built } = countingBuild();

  const plan = planRows(drawn, [row("a", "two")], sameText, build);

  expect(plan[0]?.node).not.toBe(node);
  expect(plan[0]?.node).toEqual({ label: "two" });
  expect(built).toEqual(["two"]);
});

test("syncChildren leaves kept nodes in place while the last row is replaced", () => {
  const a: Node = { label: "a" };
  const b: Node = { label: "b" };
  const live1: Node = { label: "live1" };
  const live2: Node = { label: "live2" };
  const children = new FakeChildren([a, b, live1]);

  syncChildren(children, [a, b, live2]);

  expect(children.calls).toEqual(["insertBefore live2 before live1", "removeChild live1"]);
  expect(children.children).toEqual([a, b, live2]);
});

test("syncChildren appends new rows", () => {
  const a: Node = { label: "a" };
  const b: Node = { label: "b" };
  const c: Node = { label: "c" };
  const children = new FakeChildren([a]);

  syncChildren(children, [a, b, c]);

  expect(children.calls).toEqual(["insertBefore b before null", "insertBefore c before null"]);
  expect(children.children).toEqual([a, b, c]);
});

test("syncChildren removes rows that went", () => {
  const a: Node = { label: "a" };
  const b: Node = { label: "b" };
  const c: Node = { label: "c" };
  const children = new FakeChildren([a, b, c]);

  syncChildren(children, [a]);

  expect(children.calls).toEqual(["removeChild b", "removeChild c"]);
  expect(children.children).toEqual([a]);
});

test("syncChildren puts a new leading node first", () => {
  const hidden: Node = { label: "h" };
  const a: Node = { label: "a" };
  const b: Node = { label: "b" };
  const children = new FakeChildren([a, b]);

  syncChildren(children, [hidden, a, b]);

  expect(children.calls).toEqual(["insertBefore h before a"]);
  expect(children.children).toEqual([hidden, a, b]);
});
