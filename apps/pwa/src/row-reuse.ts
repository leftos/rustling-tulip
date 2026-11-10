/** A row as drawn, with the node that draws it: the unit a chat repaint reuses. */
export interface DrawnRow<R, N> {
  readonly row: R;
  readonly node: N;
}

/**
 * The rows in order, each with the node that draws it: a row whose id was
 * drawn before and still reads the same keeps that node, and any other row is
 * built anew. Nodes of rows that left are dropped, so the list is what the
 * caller keeps for the next paint.
 */
export function planRows<R extends { readonly id: string }, N>(
  drawn: ReadonlyMap<string, DrawnRow<R, N>>,
  rows: readonly R[],
  same: (a: R, b: R) => boolean,
  build: (row: R) => N,
): DrawnRow<R, N>[] {
  return rows.map((row) => {
    const kept = drawn.get(row.id);
    return kept !== undefined && same(kept.row, row) ? kept : { row, node: build(row) };
  });
}

/** The part of a node list a child sync needs: what is there, and how to change it. */
export interface ChildList<N> {
  readonly children: ArrayLike<N>;
  insertBefore(node: N, ref: N | null): unknown;
  removeChild(node: N): unknown;
}

/**
 * Makes `parent`'s children exactly `desired`, in order, touching only what
 * differs: a node already at its place is left alone, since removing and
 * re-inserting it would reset its own scroll and any selection inside it. A
 * missing or displaced node is inserted before whatever holds its place, and
 * the children past the end are removed.
 */
export function syncChildren<N>(parent: ChildList<N>, desired: readonly N[]): void {
  for (const [at, node] of desired.entries()) {
    if (parent.children[at] === node) {
      continue;
    }
    parent.insertBefore(node, at < parent.children.length ? (parent.children[at] ?? null) : null);
  }
  while (parent.children.length > desired.length) {
    const gone = parent.children[desired.length];
    if (gone === undefined) {
      break;
    }
    parent.removeChild(gone);
  }
}
