import type { GridNode, TabEntry } from "../types";
import { tabGrid } from "../types";

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
  const grid = tabGrid(tab);
  if (!grid) return false;
  return collectPanes(grid).some((p) => p.session_id !== null);
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
    const grid = tabGrid(tab);
    if (!grid) continue;
    for (const pane of collectPanes(grid)) {
      if (pane.session_id === sessionId) {
        return { tabId: tab.id, paneId: pane.pane_id };
      }
    }
  }
  return null;
}

/**
 * Enumerate every tab+pane that references `sessionId`. A single session can
 * be open in multiple panes across multiple tabs (rare but supported by the
 * protocol); the sidebar uses this to render a pill that reveals all bindings
 * and to pick a drag-source pane.
 */
export function sessionTabBindings(
  sessionId: string,
  tabs: TabEntry[],
): Array<{ tab_id: string; tab_name: string; pane_id: string }> {
  const out: Array<{ tab_id: string; tab_name: string; pane_id: string }> = [];
  for (const tab of tabs) {
    const grid = tabGrid(tab);
    if (!grid) continue;
    for (const pane of collectPanes(grid)) {
      if (pane.session_id === sessionId) {
        out.push({ tab_id: tab.id, tab_name: tab.name, pane_id: pane.pane_id });
      }
    }
  }
  return out;
}

/**
 * First leaf pane in `tab`'s grid (left-to-right). Used by the "bind to
 * existing tab" path: split this pane and seed the new sibling with the
 * unbound session via `SplitPane { new_session_id: Some(s) }`.
 */
export function firstLeafPane(
  tab: TabEntry,
): { pane_id: string; session_id: string | null } | null {
  const grid = tabGrid(tab);
  if (!grid) return null;
  const panes = collectPanes(grid);
  return panes[0] ?? null;
}
