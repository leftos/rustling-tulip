import type {
  GridNode,
  PaneDropEdge,
  SplitDirection,
  SplitPlace,
  TabEntry,
} from "../types";
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
 * Pane count for the tab. Used by the close-confirm gate: a tab with 2+
 * panes carries grid layout that disappears on close, so users get the
 * same two-state confirm pattern even when none of the panes are bound
 * to a live session.
 */
export function tabPaneCount(tab: TabEntry): number {
  const grid = tabGrid(tab);
  if (!grid) return 0;
  return collectPanes(grid).length;
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
 * Find the tab + pane binding for a specific pane id. Used by pane pop-out
 * windows to re-resolve their source slot after tab moves/renames.
 */
export function findPaneBinding(
  tabs: TabEntry[],
  paneId: string,
): { tab: TabEntry; session_id: string | null } | null {
  for (const tab of tabs) {
    const grid = tabGrid(tab);
    if (!grid) continue;
    for (const pane of collectPanes(grid)) {
      if (pane.pane_id === paneId) {
        return { tab, session_id: pane.session_id };
      }
    }
  }
  return null;
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

/**
 * Resolve which pane should be focused when activating `tab`. Prefers the
 * caller's remembered pane id (per-tab focus memory) when it still exists
 * in the grid; otherwise falls back to the first leaf pane. Returns null
 * for tabs that don't carry a grid (e.g. diff tabs).
 */
export function resolveTabFocus(
  tab: TabEntry | undefined,
  remembered: string | undefined,
): string | null {
  if (!tab) return null;
  const grid = tabGrid(tab);
  if (!grid) return null;
  const panes = collectPanes(grid);
  if (remembered && panes.some((p) => p.pane_id === remembered)) {
    return remembered;
  }
  return panes[0]?.pane_id ?? null;
}

/// Normalized rectangle of a pane within the tab (each axis in 0..1).
/// Computed by walking the split tree and multiplying each split's ratio
/// against the running width/height — the same model `SplitRenderer` uses
/// for its CSS `fr` units, so this matches what the user actually sees.
export interface PaneRect {
  paneId: string;
  sessionId: string | null;
  x: number;
  y: number;
  width: number;
  height: number;
}

/// Compute the normalized rectangle for every pane in `grid`. Order matches
/// `collectPanes` (left-to-right walk).
export function paneRects(grid: GridNode): PaneRect[] {
  const out: PaneRect[] = [];
  walkRect(grid, 0, 0, 1, 1, out);
  return out;
}

function walkRect(
  node: GridNode,
  x: number,
  y: number,
  w: number,
  h: number,
  out: PaneRect[],
) {
  if (node.kind === "pane") {
    out.push({
      paneId: node.pane_id,
      sessionId: node.session_id,
      x,
      y,
      width: w,
      height: h,
    });
    return;
  }
  const r = node.ratio;
  if (node.direction === "horizontal") {
    walkRect(node.first, x, y, w * r, h, out);
    walkRect(node.second, x + w * r, y, w * (1 - r), h, out);
  } else {
    walkRect(node.first, x, y, w, h * r, out);
    walkRect(node.second, x, y + h * r, w, h * (1 - r), out);
  }
}

/// Look up a single pane's rectangle. `null` if the pane id is unknown.
export function paneRect(grid: GridNode, paneId: string): PaneRect | null {
  return paneRects(grid).find((r) => r.paneId === paneId) ?? null;
}

/// Pick the split direction that bisects `rect` along its longer axis.
/// Splitting a wide pane horizontally (vertical divider) and a tall pane
/// vertically (horizontal divider) keeps the resulting children closer to
/// square, which is the rule that lets the auto-placer turn a 2+1 column
/// layout into a 2x2 grid on the 4th pane.
export function balanceSplitDirection(rect: {
  width: number;
  height: number;
}): SplitDirection {
  return rect.width >= rect.height ? "horizontal" : "vertical";
}

/// Pick the pane to split for a new auto-placed pane, plus the split
/// direction that keeps the resulting layout balanced. Returns the largest
/// pane (by rendered area) and the direction from `balanceSplitDirection`,
/// with the new sibling on the `second` (right/bottom) side. Returns `null`
/// for an empty grid.
export function pickBalancedSplitTarget(grid: GridNode): {
  paneId: string;
  direction: SplitDirection;
  place: SplitPlace;
} | null {
  const rects = paneRects(grid);
  let best: PaneRect | null = null;
  for (const r of rects) {
    if (!best || r.width * r.height > best.width * best.height) {
      best = r;
    }
  }
  if (!best) return null;
  return {
    paneId: best.paneId,
    direction: balanceSplitDirection(best),
    place: "second",
  };
}

/// Pick a destination pane + edge for a `move_pane` that targets an entire
/// tab (the user dropped a pane on a tab pill, or chose "Move to" from a
/// context menu without picking a specific drop edge). Mirrors the
/// auto-placement rules used for spawning: an empty pane absorbs the drop
/// in place; otherwise the largest pane is bisected along its longer axis,
/// with the moved pane landing as the right/bottom sibling. Returns `null`
/// for an empty grid.
export function pickBalancedDropTarget(grid: GridNode): {
  paneId: string;
  edge: PaneDropEdge;
} | null {
  const panes = collectPanes(grid);
  const emptyPane = panes.find((p) => p.session_id === null);
  if (emptyPane) {
    return { paneId: emptyPane.pane_id, edge: "replace" };
  }
  const balanced = pickBalancedSplitTarget(grid);
  if (!balanced) return null;
  return {
    paneId: balanced.paneId,
    edge: balanced.direction === "horizontal" ? "right" : "bottom",
  };
}
