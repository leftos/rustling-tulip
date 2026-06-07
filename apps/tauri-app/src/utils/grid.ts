import type {
  GridNode,
  PaneDropEdge,
  SessionSnapshot,
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
 * Count of session-bound panes in the tab. `rearrange_grid` (daemon) drops
 * empty placeholder panes before rebuilding the grid, so this is the pane
 * count a rearrange actually lays out — the honest basis for labeling the
 * grid-arrangement options the user can pick.
 */
export function tabBoundPaneCount(tab: TabEntry): number {
  const grid = tabGrid(tab);
  if (!grid) return 0;
  return collectPanes(grid).filter((p) => p.session_id !== null).length;
}

/**
 * A session counts as "busy" when it's actively doing something worth
 * surfacing: running a command, awaiting permission, or still booting. Idle
 * and exited sessions are calm and don't count.
 */
export function isSessionBusy(session: SessionSnapshot): boolean {
  return (
    session.status === "working" ||
    session.status === "awaiting_input" ||
    session.status === "spawning"
  );
}

export interface TabSessionCounts {
  busy: number;
  total: number;
}

/**
 * Busy / total live-session counts for a tab. `total` counts panes bound to a
 * non-inactive session; `busy` counts the subset that are busy. Shared by the
 * OS-window title and the tab-strip (M/N) badge so the two never disagree.
 */
export function tabSessionCounts(
  tab: TabEntry,
  sessions: SessionSnapshot[],
): TabSessionCounts {
  const grid = tabGrid(tab);
  if (!grid) return { busy: 0, total: 0 };
  const byId = new Map(sessions.map((s) => [s.id, s] as const));
  let total = 0;
  let busy = 0;
  for (const pane of collectPanes(grid)) {
    if (pane.session_id === null) continue;
    const session = byId.get(pane.session_id);
    if (!session || session.is_inactive) continue;
    total += 1;
    if (isSessionBusy(session)) busy += 1;
  }
  return { busy, total };
}

/**
 * Aspect-aware column count for an N-pane "Auto" grid. Picks the column
 * count whose cells are closest to square for the given container aspect
 * ratio (width / height), preferring layouts that fill evenly (penalizing
 * each ragged/empty trailing cell) and discouraging degenerate single-row /
 * single-column shapes for non-trivial pane counts. A square container
 * (`aspect == 1`) yields a roughly square grid; a wide container biases
 * toward more columns, a tall one toward more rows.
 *
 * The `0.3` empty-cell weight is tuned so a 4-pane grid on any container
 * wider than ~1.5:1 reads as a clean 2x2 rather than a 3x2 with two phantom
 * cells: the squarer cells of a 3-wide layout (cellAspect ~1.19 vs ~1.78 at
 * 16:9) only edge out 2x2 below a ~0.20 weight. The same weight keeps 9 -> 3x3
 * and 16 -> 4x4 clean; going past ~0.34 would over-penalize empties and pull
 * 10 panes on a tall/square container from 4x3 to a wide 5x2.
 *
 * The client computes this — rather than the daemon's `cols == 0` path —
 * because only the client knows the viewport: see `paneAreaAspect`.
 */
export function bestFitGridCols(n: number, aspect: number): number {
  if (n <= 1) return 1;
  const a = aspect > 0 && Number.isFinite(aspect) ? aspect : 16 / 9;
  let best = 1;
  let bestScore = Number.POSITIVE_INFINITY;
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    const empties = rows * cols - n;
    const cellAspect = (a * rows) / cols;
    let score = Math.abs(Math.log(cellAspect)) + 0.3 * empties;
    if ((cols === 1 || cols === n) && n >= 5) {
      // A 1×N / N×1 strip rarely reads well for 5+ panes; an extreme
      // window aspect can still win it on cell shape alone.
      score += 0.4;
    }
    if (score < bestScore) {
      bestScore = score;
      best = cols;
    }
  }
  return best;
}

/**
 * Aspect ratio (width / height) of the live pane area — the grid container
 * the panes occupy. Used to drive `bestFitGridCols` for the "Auto" grid
 * actions. Falls back to the window aspect, then 16:9, when the element
 * can't be measured (e.g. nothing rendered yet).
 */
export function paneAreaAspect(): number {
  const el =
    document.querySelector(".grid-root") ?? document.querySelector(".main-pane");
  if (el) {
    const r = el.getBoundingClientRect();
    if (r.width > 0 && r.height > 0) return r.width / r.height;
  }
  if (window.innerHeight > 0) return window.innerWidth / window.innerHeight;
  return 16 / 9;
}

/// A concrete grid shape: `cols` columns per row, `rows` rows total.
export interface GridArrangement {
  cols: number;
  rows: number;
}

/**
 * Enumerate the grid shapes strictly between "Stacked" (1 column) and "Side
 * by side" (N columns) for an N-pane tab: every column count from 2 to N-1.
 * `rows` is `ceil(N / cols)`, matching the daemon's row-major chunking in
 * `build_grid`. Returns an empty list for N < 3 (no in-between shape exists).
 */
export function gridArrangements(n: number): GridArrangement[] {
  const out: GridArrangement[] = [];
  for (let cols = 2; cols <= n - 1; cols++) {
    out.push({ cols, rows: Math.ceil(n / cols) });
  }
  return out;
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

/**
 * Walk `grid` to find `paneId`'s immediate split sibling subtree, then return
 * the first session-bound pane in that subtree (left-to-right). Used by the
 * empty-pane "spawn here" path to pre-select the repo/ws of the pane the user
 * just split off of. Returns `null` when `paneId` has no parent split (root
 * pane) or no sibling pane carries a session.
 */
export function findSplitSiblingSession(
  grid: GridNode,
  paneId: string,
): string | null {
  return walkForSibling(grid, paneId);
}

function walkForSibling(node: GridNode, paneId: string): string | null {
  if (node.kind === "pane") return null;
  if (node.first.kind === "pane" && node.first.pane_id === paneId) {
    return firstSessionInSubtree(node.second);
  }
  if (node.second.kind === "pane" && node.second.pane_id === paneId) {
    return firstSessionInSubtree(node.first);
  }
  return (
    walkForSibling(node.first, paneId) ??
    walkForSibling(node.second, paneId)
  );
}

function firstSessionInSubtree(node: GridNode): string | null {
  for (const p of collectPanes(node)) {
    if (p.session_id !== null) return p.session_id;
  }
  return null;
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
