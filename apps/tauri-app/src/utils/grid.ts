import type { GridNode } from "../types";

/**
 * Walk a grid tree and return every leaf pane in left-then-right order.
 */
export function collectPanes(
  node: GridNode,
): Array<{ pane_id: string; session_id: string | null }> {
  const out: Array<{ pane_id: string; session_id: string | null }> = [];
  walk(node, out);
  return out;
}

function walk(
  node: GridNode,
  out: Array<{ pane_id: string; session_id: string | null }>,
) {
  if (node.kind === "pane") {
    out.push({ pane_id: node.pane_id, session_id: node.session_id });
    return;
  }
  walk(node.first, out);
  walk(node.second, out);
}
