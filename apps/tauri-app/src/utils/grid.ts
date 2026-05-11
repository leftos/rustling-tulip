import type { GridNode, TabEntry } from "../types";

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

/**
 * Whether any pane in this grid is bound to a session. Used by the tab
 * close-confirm gate (close-immediately when all panes are empty, ask for
 * confirm when at least one carries a session).
 */
export function tabHasBoundSessions(tab: TabEntry): boolean {
  return collectPanes(tab.grid).some((p) => p.session_id !== null);
}

/**
 * Find the first tab whose grid contains a pane bound to `sessionId`. Returns
 * the tab id and the pane id, or `null` if no tab references the session.
 * Used to route a sidebar click back to an already-open tab instead of
 * spawning a duplicate.
 */
export function findTabContainingSession(
  tabs: TabEntry[],
  sessionId: string,
): { tabId: string; paneId: string } | null {
  for (const tab of tabs) {
    for (const pane of collectPanes(tab.grid)) {
      if (pane.session_id === sessionId) {
        return { tabId: tab.id, paneId: pane.pane_id };
      }
    }
  }
  return null;
}
