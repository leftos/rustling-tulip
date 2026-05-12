//! Pure tree mutations on [`GridNode`] plus multi-tab helpers (merge / extract).
//!
//! All operations are pure on the in-memory tree; persistence and broadcast
//! happen in the dispatch layer (`server.rs`). The helpers panic-free and use
//! `anyhow::Result` to surface missing-id errors.

use anyhow::{Context as _, anyhow};
use chrono::Utc;
use protocol::{
    GridNode, MergeLayout, PaneDropEdge, RearrangeLayout, SplitDirection, SplitPlace, TabContent,
    TabEntry,
};
use uuid::Uuid;

const MIN_RATIO: f32 = 0.05;
const MAX_RATIO: f32 = 0.95;
const DEFAULT_RATIO: f32 = 0.5;

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn placeholder() -> GridNode {
    GridNode::Pane {
        pane_id: String::new(),
        session_id: None,
    }
}

fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_nan() {
        DEFAULT_RATIO
    } else {
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    }
}

/// Build a fresh single-pane tab. When `name` is `None`, the helper picks
/// the next free default name (`"Tab"`, `"Tab 2"`, `"Tab 3"`, ...) based on
/// `existing` so the tab bar doesn't render `Tab │ Tab │ Tab`.
pub fn make_tab(
    name: Option<String>,
    initial_session_id: Option<String>,
    existing: &[TabEntry],
) -> TabEntry {
    let id = new_id();
    let pane_id = new_id();
    TabEntry {
        id,
        name: name.unwrap_or_else(|| next_default_name(existing)),
        content: TabContent::Grid {
            grid: GridNode::Pane {
                pane_id,
                session_id: initial_session_id,
            },
        },
        created_at: Utc::now(),
    }
}

/// Pick the smallest positive integer N such that neither `"Tab"` (for
/// N == 1) nor `"Tab N"` (for N >= 2) already appears in `existing`'s
/// names. Lets a closed-and-reopened tab reuse the freed slot instead of
/// growing monotonically.
fn next_default_name(existing: &[TabEntry]) -> String {
    let mut used: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for t in existing {
        if t.name == "Tab" {
            used.insert(1);
        } else if let Some(rest) = t.name.strip_prefix("Tab ")
            && let Ok(n) = rest.parse::<u32>()
        {
            used.insert(n);
        }
    }
    let mut n: u32 = 1;
    while used.contains(&n) {
        n += 1;
    }
    if n == 1 {
        "Tab".to_string()
    } else {
        format!("Tab {n}")
    }
}

/// Borrow the grid of `tab` or fail with a clear error when the tab is
/// not a grid kind. Pane-level operations only make sense on grid tabs.
pub fn grid_or_err(tab: &TabEntry) -> anyhow::Result<&GridNode> {
    tab.grid()
        .ok_or_else(|| anyhow!("tab {} is not a grid tab", tab.id))
}

/// Mutable counterpart to [`grid_or_err`].
pub fn grid_or_err_mut(tab: &mut TabEntry) -> anyhow::Result<&mut GridNode> {
    let id = tab.id.clone();
    tab.grid_mut()
        .ok_or_else(|| anyhow!("tab {id} is not a grid tab"))
}

/// Find the tab by id, returning a mutable reference.
pub fn find_tab_mut<'a>(
    tabs: &'a mut [TabEntry],
    tab_id: &str,
) -> anyhow::Result<&'a mut TabEntry> {
    tabs.iter_mut()
        .find(|t| t.id == tab_id)
        .with_context(|| format!("unknown tab: {tab_id}"))
}

/// True if `target` matches any pane (leaf) in the tree.
pub fn pane_exists(node: &GridNode, target: &str) -> bool {
    match node {
        GridNode::Pane { pane_id, .. } => pane_id == target,
        GridNode::Split { first, second, .. } => {
            pane_exists(first, target) || pane_exists(second, target)
        }
    }
}

/// Walk all panes (leaves) in left-then-right order and return their
/// (`pane_id`, `session_id`) tuples.
pub fn collect_panes(node: &GridNode) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    collect_in(node, &mut out);
    out
}

fn collect_in(node: &GridNode, out: &mut Vec<(String, Option<String>)>) {
    match node {
        GridNode::Pane {
            pane_id,
            session_id,
        } => {
            out.push((pane_id.clone(), session_id.clone()));
        }
        GridNode::Split { first, second, .. } => {
            collect_in(first, out);
            collect_in(second, out);
        }
    }
}

/// Split the pane with id `target` along `direction`. The new (empty or
/// session-seeded) pane occupies `place`; the existing pane occupies the
/// other side. Returns the new pane's id.
pub fn split_pane(
    node: &mut GridNode,
    target: &str,
    direction: SplitDirection,
    place: SplitPlace,
    new_session_id: Option<String>,
) -> anyhow::Result<String> {
    let new_pane_id = new_id();
    if split_in(node, target, direction, place, new_session_id, &new_pane_id) {
        Ok(new_pane_id)
    } else {
        Err(anyhow!("pane not found: {target}"))
    }
}

fn split_in(
    node: &mut GridNode,
    target: &str,
    direction: SplitDirection,
    place: SplitPlace,
    new_session_id: Option<String>,
    new_pane_id: &str,
) -> bool {
    if matches!(node, GridNode::Pane { pane_id, .. } if pane_id == target) {
        let existing = std::mem::replace(node, placeholder());
        let new_pane = GridNode::Pane {
            pane_id: new_pane_id.to_string(),
            session_id: new_session_id,
        };
        let (first, second) = match place {
            SplitPlace::First => (Box::new(new_pane), Box::new(existing)),
            SplitPlace::Second => (Box::new(existing), Box::new(new_pane)),
        };
        *node = GridNode::Split {
            direction,
            ratio: DEFAULT_RATIO,
            first,
            second,
        };
        return true;
    }
    if let GridNode::Split { first, second, .. } = node {
        if split_in(
            first,
            target,
            direction,
            place,
            new_session_id.clone(),
            new_pane_id,
        ) {
            return true;
        }
        return split_in(
            second,
            target,
            direction,
            place,
            new_session_id,
            new_pane_id,
        );
    }
    false
}

/// Close the pane with id `target`. Returns `Ok(true)` when the close
/// emptied the tab (caller should remove the tab); `Ok(false)` otherwise.
pub fn close_pane(grid: &mut GridNode, target: &str) -> anyhow::Result<bool> {
    if matches!(grid, GridNode::Pane { pane_id, .. } if pane_id == target) {
        return Ok(true);
    }
    if remove_pane_in_split(grid, target) {
        Ok(false)
    } else {
        Err(anyhow!("pane not found: {target}"))
    }
}

/// When `target` is found as a direct or descendant Pane under a Split,
/// collapse the parent Split into the sibling. Returns true if removal
/// happened in this subtree.
fn remove_pane_in_split(node: &mut GridNode, target: &str) -> bool {
    let GridNode::Split { first, second, .. } = node else {
        return false;
    };
    let first_match = matches!(&**first, GridNode::Pane { pane_id, .. } if pane_id == target);
    let second_match = matches!(&**second, GridNode::Pane { pane_id, .. } if pane_id == target);
    if first_match {
        let sibling = std::mem::replace(second.as_mut(), placeholder());
        *node = sibling;
        return true;
    }
    if second_match {
        let sibling = std::mem::replace(first.as_mut(), placeholder());
        *node = sibling;
        return true;
    }
    if remove_pane_in_split(first, target) {
        return true;
    }
    remove_pane_in_split(second, target)
}

/// Update a single split's ratio. `split_path` is a sequence of 0/1 indices
/// from the tab root (0 = first child, 1 = second child) terminating at the
/// Split node to modify.
pub fn set_pane_ratio(grid: &mut GridNode, split_path: &[u8], ratio: f32) -> anyhow::Result<()> {
    let mut node = grid;
    for (depth, idx) in split_path.iter().enumerate() {
        let GridNode::Split { first, second, .. } = node else {
            return Err(anyhow!("split path expects Split at depth {depth}"));
        };
        node = match *idx {
            0 => first.as_mut(),
            1 => second.as_mut(),
            other => return Err(anyhow!("invalid split path index: {other}")),
        };
    }
    let GridNode::Split { ratio: r, .. } = node else {
        return Err(anyhow!("split path does not terminate at a Split"));
    };
    *r = clamp_ratio(ratio);
    Ok(())
}

/// Replace a pane's session reference without altering topology.
pub fn replace_pane_session(
    grid: &mut GridNode,
    target: &str,
    session_id: Option<String>,
) -> anyhow::Result<()> {
    if replace_session_in(grid, target, session_id) {
        Ok(())
    } else {
        Err(anyhow!("pane not found: {target}"))
    }
}

fn replace_session_in(node: &mut GridNode, target: &str, sid: Option<String>) -> bool {
    match node {
        GridNode::Pane {
            pane_id,
            session_id,
        } if pane_id == target => {
            *session_id = sid;
            true
        }
        GridNode::Pane { .. } => false,
        GridNode::Split { first, second, .. } => {
            if replace_session_in(first, target, sid.clone()) {
                true
            } else {
                replace_session_in(second, target, sid)
            }
        }
    }
}

/// Set every pane referencing `removed_session_id` to `None`. Returns true if
/// at least one pane was modified.
pub fn prune_session(grid: &mut GridNode, removed_session_id: &str) -> bool {
    match grid {
        GridNode::Pane { session_id, .. } => {
            if session_id.as_deref() == Some(removed_session_id) {
                *session_id = None;
                true
            } else {
                false
            }
        }
        GridNode::Split { first, second, .. } => {
            let a = prune_session(first, removed_session_id);
            let b = prune_session(second, removed_session_id);
            a || b
        }
    }
}

/// Clear every pane whose `session_id` is not in `keep`. Used on daemon
/// startup to drop dangling references to sessions that did not survive the
/// restart. Returns true if at least one pane was modified.
pub fn prune_sessions_not_in(
    grid: &mut GridNode,
    keep: &std::collections::HashSet<String>,
) -> bool {
    match grid {
        GridNode::Pane { session_id, .. } => {
            if let Some(id) = session_id.as_deref()
                && !keep.contains(id)
            {
                *session_id = None;
                true
            } else {
                false
            }
        }
        GridNode::Split { first, second, .. } => {
            let a = prune_sessions_not_in(first, keep);
            let b = prune_sessions_not_in(second, keep);
            a || b
        }
    }
}

/// True iff at least one pane in the grid is still bound to a session id.
/// A tab where every pane has `session_id == None` carries no recoverable
/// state across a daemon restart, so the supervisor drops it on startup.
#[must_use]
pub fn has_any_session(grid: &GridNode) -> bool {
    match grid {
        GridNode::Pane { session_id, .. } => session_id.is_some(),
        GridNode::Split { first, second, .. } => has_any_session(first) || has_any_session(second),
    }
}

/// Extract a pane (leaf) from the grid, collapsing parent splits as needed.
/// Returns the extracted Pane node plus a flag indicating whether the source
/// tab is now empty (caller should remove the tab).
pub fn extract_pane(grid: &mut GridNode, target: &str) -> anyhow::Result<(GridNode, bool)> {
    if matches!(grid, GridNode::Pane { pane_id, .. } if pane_id == target) {
        let extracted = std::mem::replace(grid, placeholder());
        return Ok((extracted, true));
    }
    extract_in_split(grid, target)
        .map(|node| (node, false))
        .ok_or_else(|| anyhow!("pane not found: {target}"))
}

fn extract_in_split(node: &mut GridNode, target: &str) -> Option<GridNode> {
    let GridNode::Split { first, second, .. } = node else {
        return None;
    };
    if matches!(&**first, GridNode::Pane { pane_id, .. } if pane_id == target) {
        let extracted = std::mem::replace(first.as_mut(), placeholder());
        let sibling = std::mem::replace(second.as_mut(), placeholder());
        *node = sibling;
        return Some(extracted);
    }
    if matches!(&**second, GridNode::Pane { pane_id, .. } if pane_id == target) {
        let extracted = std::mem::replace(second.as_mut(), placeholder());
        let sibling = std::mem::replace(first.as_mut(), placeholder());
        *node = sibling;
        return Some(extracted);
    }
    if let Some(extracted) = extract_in_split(first, target) {
        return Some(extracted);
    }
    extract_in_split(second, target)
}

/// Insert `source` adjacent to the pane with id `dst_target` along `edge`.
/// `Edge::Replace` swaps `dst_target`'s `session_id` with the source pane's
/// `session_id`; the source pane id is discarded.
pub fn insert_adjacent(
    grid: &mut GridNode,
    dst_target: &str,
    edge: PaneDropEdge,
    source: GridNode,
) -> anyhow::Result<()> {
    if !pane_exists(grid, dst_target) {
        return Err(anyhow!("destination pane not found: {dst_target}"));
    }
    insert_in(grid, dst_target, edge, source);
    Ok(())
}

fn insert_in(node: &mut GridNode, target: &str, edge: PaneDropEdge, source: GridNode) {
    if matches!(node, GridNode::Pane { pane_id, .. } if pane_id == target) {
        match edge {
            PaneDropEdge::Replace => {
                let dst_pane_id = match node {
                    GridNode::Pane { pane_id, .. } => pane_id.clone(),
                    GridNode::Split { .. } => return,
                };
                let src_sid = match source {
                    GridNode::Pane { session_id, .. } => session_id,
                    GridNode::Split { .. } => None,
                };
                *node = GridNode::Pane {
                    pane_id: dst_pane_id,
                    session_id: src_sid,
                };
            }
            edge => {
                let direction = match edge {
                    PaneDropEdge::Left | PaneDropEdge::Right => SplitDirection::Horizontal,
                    PaneDropEdge::Top | PaneDropEdge::Bottom => SplitDirection::Vertical,
                    PaneDropEdge::Replace => unreachable!("handled above"),
                };
                let existing = std::mem::replace(node, placeholder());
                let (first, second) = match edge {
                    PaneDropEdge::Left | PaneDropEdge::Top => {
                        (Box::new(source), Box::new(existing))
                    }
                    PaneDropEdge::Right | PaneDropEdge::Bottom => {
                        (Box::new(existing), Box::new(source))
                    }
                    PaneDropEdge::Replace => unreachable!("handled above"),
                };
                *node = GridNode::Split {
                    direction,
                    ratio: DEFAULT_RATIO,
                    first,
                    second,
                };
            }
        }
        return;
    }
    if let GridNode::Split { first, second, .. } = node {
        if pane_exists(first, target) {
            insert_in(first, target, edge, source);
        } else {
            insert_in(second, target, edge, source);
        }
    }
}

/// Build a balanced split tree from a list of leaves laid out along
/// `direction`. Returns `None` for an empty input.
pub fn build_balanced(
    panes: &[(String, Option<String>)],
    direction: SplitDirection,
) -> Option<GridNode> {
    if panes.is_empty() {
        return None;
    }
    Some(build_balanced_inner(panes, direction))
}

fn build_balanced_inner(panes: &[(String, Option<String>)], direction: SplitDirection) -> GridNode {
    debug_assert!(!panes.is_empty(), "caller guards against empty input");
    if panes.len() == 1 {
        let (pid, sid) = &panes[0];
        return GridNode::Pane {
            pane_id: pid.clone(),
            session_id: sid.clone(),
        };
    }
    let n = panes.len();
    let mid = n / 2;
    let first = build_balanced_inner(&panes[..mid], direction);
    let second = build_balanced_inner(&panes[mid..], direction);
    #[expect(
        clippy::cast_precision_loss,
        reason = "tab pane counts are tiny; precision loss is irrelevant for ratios"
    )]
    let ratio = (mid as f32) / (n as f32);
    GridNode::Split {
        direction,
        ratio: clamp_ratio(ratio),
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// Pick a sensible column count for an auto-grid of `n` panes. Returns
/// `ceil(sqrt(n))` so the result is the smallest grid wider than (or as
/// wide as) it is tall. `n == 0` returns 1 so callers can divide without
/// guarding.
#[must_use]
pub fn auto_grid_cols(n: usize) -> u32 {
    if n == 0 {
        return 1;
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "pane counts are tiny; precision loss is irrelevant for sqrt"
    )]
    let cols = (n as f64).sqrt().ceil() as u32;
    cols.max(1)
}

/// Build a true 2D grid: `cols` columns per row, rows stacked vertically.
/// The last row may be short. Returns `None` for an empty input.
///
/// `cols == 0` is rewritten to `auto_grid_cols(panes.len())` so callers
/// can pass the daemon's autopick straight through.
pub fn build_grid(panes: &[(String, Option<String>)], cols: u32) -> Option<GridNode> {
    if panes.is_empty() {
        return None;
    }
    let cols = if cols == 0 {
        auto_grid_cols(panes.len())
    } else {
        cols
    };
    let cols_usize = cols as usize;
    let mut row_nodes: Vec<GridNode> = Vec::with_capacity(panes.len().div_ceil(cols_usize));
    for chunk in panes.chunks(cols_usize) {
        let row = build_balanced(chunk, SplitDirection::Horizontal)?;
        row_nodes.push(row);
    }
    Some(stack_balanced(row_nodes, SplitDirection::Vertical))
}

/// Build a balanced split tree over a list of pre-built `GridNode` children
/// joined by `direction`. Differs from [`build_balanced`] in that the
/// leaves are already arbitrary nodes (not pane tuples).
fn stack_balanced(mut nodes: Vec<GridNode>, direction: SplitDirection) -> GridNode {
    debug_assert!(!nodes.is_empty(), "caller guards against empty input");
    if nodes.len() == 1 {
        return nodes.remove(0);
    }
    let n = nodes.len();
    let mid = n / 2;
    let second_half = nodes.split_off(mid);
    let first = stack_balanced(nodes, direction);
    let second = stack_balanced(second_half, direction);
    #[expect(
        clippy::cast_precision_loss,
        reason = "row counts are tiny; precision loss is irrelevant for ratios"
    )]
    let ratio = (mid as f32) / (n as f32);
    GridNode::Split {
        direction,
        ratio: clamp_ratio(ratio),
        first: Box::new(first),
        second: Box::new(second),
    }
}

/// Rebuild a tab's grid using `layout`, preserving every pane id (and its
/// `session_id` binding). Pane order is left-to-right as walked by
/// [`collect_panes`]. Returns the new grid; caller is responsible for
/// writing it back to the tab and broadcasting `TabUpdated`.
pub fn rearrange_grid(grid: &GridNode, layout: RearrangeLayout) -> anyhow::Result<GridNode> {
    let panes = collect_panes(grid);
    if panes.is_empty() {
        return Err(anyhow!("cannot rearrange an empty grid"));
    }
    let new = match layout {
        RearrangeLayout::Horizontal | RearrangeLayout::Balanced => {
            build_balanced(&panes, SplitDirection::Horizontal)
        }
        RearrangeLayout::Vertical => build_balanced(&panes, SplitDirection::Vertical),
        RearrangeLayout::Grid { cols } => build_grid(&panes, cols),
    };
    new.ok_or_else(|| anyhow!("rearrange produced empty tab (unreachable)"))
}

/// Merge multiple tabs into a single new tab. Removes the source tabs from
/// `tabs` and returns the new tab. Caller is responsible for broadcasting
/// `TabRemoved` for each consumed id and `TabUpdated` for the survivor.
pub fn merge_tabs(
    tabs: &mut Vec<TabEntry>,
    tab_ids: &[String],
    name: Option<String>,
    layout: MergeLayout,
) -> anyhow::Result<TabEntry> {
    if tab_ids.len() < 2 {
        return Err(anyhow!("merge requires at least two tabs"));
    }
    let mut collected: Vec<(String, Option<String>)> = Vec::new();
    for id in tab_ids {
        let entry = tabs
            .iter()
            .find(|t| &t.id == id)
            .with_context(|| format!("unknown tab: {id}"))?;
        let grid = grid_or_err(entry)?;
        collected.extend(collect_panes(grid));
    }
    let direction = match layout {
        MergeLayout::TileHorizontal => SplitDirection::Horizontal,
        MergeLayout::TileVertical => SplitDirection::Vertical,
    };
    let grid = build_balanced(&collected, direction)
        .ok_or_else(|| anyhow!("merge would produce empty tab"))?;
    tabs.retain(|t| !tab_ids.contains(&t.id));
    let resolved_name = name.unwrap_or_else(|| next_default_name(tabs));
    let entry = TabEntry {
        id: new_id(),
        name: resolved_name,
        content: TabContent::Grid { grid },
        created_at: Utc::now(),
    };
    tabs.push(entry.clone());
    Ok(entry)
}

/// Move a set of panes out of `source_tab_id` into a fresh tab. Returns the
/// new tab. If extracting empties the source tab, the source is removed
/// (caller must broadcast `TabRemoved` for the source as well).
pub fn extract_to_new_tab(
    tabs: &mut Vec<TabEntry>,
    source_tab_id: &str,
    pane_ids: &[String],
    name: Option<String>,
) -> anyhow::Result<(TabEntry, bool)> {
    if pane_ids.is_empty() {
        return Err(anyhow!("extract requires at least one pane"));
    }
    let source = find_tab_mut(tabs, source_tab_id)?;
    let source_grid = grid_or_err_mut(source)?;
    let mut extracted: Vec<(String, Option<String>)> = Vec::with_capacity(pane_ids.len());
    let mut source_empty = false;
    for pid in pane_ids {
        let (node, empty) = extract_pane(source_grid, pid)?;
        let GridNode::Pane {
            pane_id,
            session_id,
        } = node
        else {
            return Err(anyhow!("extracted node is not a pane: {pid}"));
        };
        extracted.push((pane_id, session_id));
        if empty {
            source_empty = true;
            break;
        }
    }
    if source_empty {
        tabs.retain(|t| t.id != source_tab_id);
    }
    let grid = build_balanced(&extracted, SplitDirection::Horizontal)
        .ok_or_else(|| anyhow!("extract produced empty tab"))?;
    let resolved_name = name.unwrap_or_else(|| next_default_name(tabs));
    let entry = TabEntry {
        id: new_id(),
        name: resolved_name,
        content: TabContent::Grid { grid },
        created_at: Utc::now(),
    };
    tabs.push(entry.clone());
    Ok((entry, source_empty))
}

/// Validate `ordered_ids` is a permutation of the existing tab ids and apply
/// the order in place.
pub fn reorder_tabs(tabs: &mut [TabEntry], ordered_ids: &[String]) -> anyhow::Result<()> {
    if ordered_ids.len() != tabs.len() {
        return Err(anyhow!(
            "reorder set mismatch: {} ids given, {} tabs known",
            ordered_ids.len(),
            tabs.len()
        ));
    }
    for id in ordered_ids {
        if !tabs.iter().any(|t| &t.id == id) {
            return Err(anyhow!("unknown tab in reorder set: {id}"));
        }
    }
    // Sort by index in `ordered_ids`. Safe because we just verified every id
    // is present; `position()` always returns `Some`.
    tabs.sort_by_key(|t| {
        ordered_ids
            .iter()
            .position(|id| id == &t.id)
            .unwrap_or(usize::MAX)
    });
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect/panic; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::SplitDirection::{Horizontal, Vertical};

    fn pane(id: &str, sid: Option<&str>) -> GridNode {
        GridNode::Pane {
            pane_id: id.to_string(),
            session_id: sid.map(str::to_string),
        }
    }

    fn split(dir: SplitDirection, ratio: f32, a: GridNode, b: GridNode) -> GridNode {
        GridNode::Split {
            direction: dir,
            ratio,
            first: Box::new(a),
            second: Box::new(b),
        }
    }

    #[test]
    fn split_pane_inserts_new_pane_on_chosen_side() {
        let mut grid = pane("p1", Some("s1"));
        let new_id = split_pane(
            &mut grid,
            "p1",
            Horizontal,
            SplitPlace::Second,
            Some("s2".into()),
        )
        .expect("split");
        let GridNode::Split {
            direction,
            ratio,
            first,
            second,
        } = grid
        else {
            panic!("expected Split");
        };
        assert_eq!(direction, Horizontal);
        assert!((ratio - DEFAULT_RATIO).abs() < f32::EPSILON);
        assert!(matches!(&*first, GridNode::Pane { pane_id, session_id }
                          if pane_id == "p1" && session_id.as_deref() == Some("s1")));
        assert!(matches!(&*second, GridNode::Pane { pane_id, session_id }
                           if pane_id == &new_id && session_id.as_deref() == Some("s2")));
    }

    #[test]
    fn split_pane_first_place_puts_new_pane_first() {
        let mut grid = pane("p1", None);
        let new_id = split_pane(&mut grid, "p1", Vertical, SplitPlace::First, None).expect("split");
        let GridNode::Split { first, second, .. } = grid else {
            panic!("expected Split");
        };
        assert!(matches!(&*first, GridNode::Pane { pane_id, .. } if pane_id == &new_id));
        assert!(matches!(&*second, GridNode::Pane { pane_id, .. } if pane_id == "p1"));
    }

    #[test]
    fn split_pane_unknown_target_errors() {
        let mut grid = pane("p1", None);
        let err = split_pane(&mut grid, "missing", Horizontal, SplitPlace::Second, None);
        assert!(err.is_err());
    }

    #[test]
    fn close_pane_root_returns_empty_flag() {
        let mut grid = pane("p1", None);
        let empty = close_pane(&mut grid, "p1").expect("close");
        assert!(empty);
    }

    #[test]
    fn close_pane_collapses_parent_split() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        );
        let empty = close_pane(&mut grid, "p1").expect("close");
        assert!(!empty);
        assert!(matches!(grid, GridNode::Pane { pane_id, session_id }
                          if pane_id == "p2" && session_id.as_deref() == Some("s2")));
    }

    #[test]
    fn close_pane_deep_target() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", None),
            split(Vertical, 0.5, pane("p2", None), pane("p3", None)),
        );
        let empty = close_pane(&mut grid, "p3").expect("close");
        assert!(!empty);
        let GridNode::Split { first, second, .. } = &grid else {
            panic!("expected Split");
        };
        assert!(matches!(&**first, GridNode::Pane { pane_id, .. } if pane_id == "p1"));
        assert!(matches!(&**second, GridNode::Pane { pane_id, .. } if pane_id == "p2"));
    }

    #[test]
    fn set_pane_ratio_descends_path() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", None),
            split(Vertical, 0.5, pane("p2", None), pane("p3", None)),
        );
        set_pane_ratio(&mut grid, &[1], 0.8).expect("ratio");
        let GridNode::Split { second, .. } = &grid else {
            panic!("expected Split");
        };
        let GridNode::Split { ratio, .. } = &**second else {
            panic!("expected nested Split");
        };
        assert!((*ratio - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn set_pane_ratio_clamps() {
        let mut grid = split(Horizontal, 0.5, pane("p1", None), pane("p2", None));
        set_pane_ratio(&mut grid, &[], 2.0).expect("ratio");
        let GridNode::Split { ratio, .. } = &grid else {
            panic!("expected Split");
        };
        assert!((*ratio - MAX_RATIO).abs() < f32::EPSILON);
    }

    #[test]
    fn replace_pane_session_finds_target() {
        let mut grid = split(Horizontal, 0.5, pane("p1", None), pane("p2", Some("s2")));
        replace_pane_session(&mut grid, "p2", None).expect("replace");
        let GridNode::Split { second, .. } = &grid else {
            panic!("expected Split");
        };
        assert!(
            matches!(&**second, GridNode::Pane { session_id: None, pane_id } if pane_id == "p2")
        );
    }

    #[test]
    fn prune_session_clears_matching_panes_only() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", Some("s-doomed")),
            split(
                Vertical,
                0.5,
                pane("p2", Some("s-doomed")),
                pane("p3", Some("s-survives")),
            ),
        );
        let changed = prune_session(&mut grid, "s-doomed");
        assert!(changed);
        assert!(!prune_session(&mut grid, "s-doomed"));
        let GridNode::Split { first, second, .. } = &grid else {
            panic!("expected Split");
        };
        assert!(matches!(
            &**first,
            GridNode::Pane {
                session_id: None,
                ..
            }
        ));
        let GridNode::Split {
            first: inner_first,
            second: inner_second,
            ..
        } = &**second
        else {
            panic!("expected nested Split");
        };
        assert!(matches!(
            &**inner_first,
            GridNode::Pane {
                session_id: None,
                ..
            }
        ));
        assert!(matches!(&**inner_second, GridNode::Pane { session_id, .. }
                                    if session_id.as_deref() == Some("s-survives")));
    }

    #[test]
    fn prune_sessions_not_in_clears_unmatched_panes_only() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", Some("s-live")),
            split(Vertical, 0.5, pane("p2", Some("s-dead")), pane("p3", None)),
        );
        let mut keep = std::collections::HashSet::new();
        keep.insert("s-live".to_string());
        assert!(prune_sessions_not_in(&mut grid, &keep));
        assert!(!prune_sessions_not_in(&mut grid, &keep));
        let panes = collect_panes(&grid);
        let by_pane: std::collections::HashMap<_, _> = panes.into_iter().collect();
        assert_eq!(by_pane.get("p1"), Some(&Some("s-live".to_string())));
        assert_eq!(by_pane.get("p2"), Some(&None));
        assert_eq!(by_pane.get("p3"), Some(&None));
    }

    #[test]
    fn has_any_session_distinguishes_bound_from_empty() {
        let bound = split(
            Horizontal,
            0.5,
            pane("p1", None),
            pane("p2", Some("s-live")),
        );
        assert!(has_any_session(&bound));
        let empty = split(Horizontal, 0.5, pane("p1", None), pane("p2", None));
        assert!(!has_any_session(&empty));
    }

    #[test]
    fn extract_pane_returns_node_and_collapses() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        );
        let (extracted, empty) = extract_pane(&mut grid, "p1").expect("extract");
        assert!(!empty);
        assert!(matches!(extracted, GridNode::Pane { pane_id, session_id }
                              if pane_id == "p1" && session_id.as_deref() == Some("s1")));
        assert!(matches!(grid, GridNode::Pane { pane_id, .. } if pane_id == "p2"));
    }

    #[test]
    fn extract_pane_from_root_marks_empty() {
        let mut grid = pane("only", None);
        let (_, empty) = extract_pane(&mut grid, "only").expect("extract");
        assert!(empty);
    }

    #[test]
    fn insert_adjacent_right_creates_horizontal_split() {
        let mut grid = pane("dst", Some("sd"));
        insert_adjacent(
            &mut grid,
            "dst",
            PaneDropEdge::Right,
            pane("src", Some("ss")),
        )
        .expect("insert");
        let GridNode::Split {
            direction,
            first,
            second,
            ..
        } = grid
        else {
            panic!("expected Split");
        };
        assert_eq!(direction, Horizontal);
        assert!(matches!(&*first, GridNode::Pane { pane_id, .. } if pane_id == "dst"));
        assert!(matches!(&*second, GridNode::Pane { pane_id, .. } if pane_id == "src"));
    }

    #[test]
    fn insert_adjacent_top_creates_vertical_split() {
        let mut grid = pane("dst", None);
        insert_adjacent(&mut grid, "dst", PaneDropEdge::Top, pane("src", Some("ss")))
            .expect("insert");
        let GridNode::Split {
            direction,
            first,
            second,
            ..
        } = grid
        else {
            panic!("expected Split");
        };
        assert_eq!(direction, Vertical);
        assert!(matches!(&*first, GridNode::Pane { pane_id, .. } if pane_id == "src"));
        assert!(matches!(&*second, GridNode::Pane { pane_id, .. } if pane_id == "dst"));
    }

    #[test]
    fn insert_adjacent_replace_swaps_session_only() {
        let mut grid = pane("dst", Some("sd"));
        insert_adjacent(
            &mut grid,
            "dst",
            PaneDropEdge::Replace,
            pane("src", Some("ss")),
        )
        .expect("insert");
        assert!(matches!(grid, GridNode::Pane { pane_id, session_id }
                          if pane_id == "dst" && session_id.as_deref() == Some("ss")));
    }

    #[test]
    fn build_balanced_three_panes_left_leans() {
        let panes = vec![
            ("a".to_string(), None),
            ("b".to_string(), None),
            ("c".to_string(), None),
        ];
        let tree = build_balanced(&panes, Horizontal).expect("non-empty");
        // 3-pane balanced: mid=1 → first=a, second=(b,c)
        let GridNode::Split { first, second, .. } = tree else {
            panic!("expected Split");
        };
        assert!(matches!(&*first, GridNode::Pane { pane_id, .. } if pane_id == "a"));
        let GridNode::Split {
            first: f2,
            second: s2,
            ..
        } = &*second
        else {
            panic!("expected nested Split");
        };
        assert!(matches!(&**f2, GridNode::Pane { pane_id, .. } if pane_id == "b"));
        assert!(matches!(&**s2, GridNode::Pane { pane_id, .. } if pane_id == "c"));
    }

    fn make_tab_with(id: &str, name: &str, grid: GridNode) -> TabEntry {
        TabEntry {
            id: id.to_string(),
            name: name.to_string(),
            content: TabContent::Grid { grid },
            created_at: Utc::now(),
        }
    }

    #[test]
    fn next_default_name_picks_smallest_free() {
        let empty: Vec<TabEntry> = vec![];
        assert_eq!(next_default_name(&empty), "Tab");

        let with_first = vec![make_tab_with("t1", "Tab", pane("p1", None))];
        assert_eq!(next_default_name(&with_first), "Tab 2");

        let with_gap = vec![
            make_tab_with("t1", "Tab", pane("p1", None)),
            make_tab_with("t3", "Tab 3", pane("p3", None)),
        ];
        assert_eq!(next_default_name(&with_gap), "Tab 2");

        // Custom names don't reserve slots — the user named those by hand.
        let custom = vec![
            make_tab_with("t1", "scratch", pane("p1", None)),
            make_tab_with("t2", "review", pane("p2", None)),
        ];
        assert_eq!(next_default_name(&custom), "Tab");
    }

    #[test]
    fn merge_tabs_concatenates_panes_and_removes_sources() {
        let mut tabs = vec![
            make_tab_with("t1", "A", pane("a1", Some("sa1"))),
            make_tab_with(
                "t2",
                "B",
                split(Horizontal, 0.5, pane("b1", None), pane("b2", None)),
            ),
        ];
        let merged = merge_tabs(
            &mut tabs,
            &["t1".to_string(), "t2".to_string()],
            Some("Merged".to_string()),
            MergeLayout::TileVertical,
        )
        .expect("merge");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, merged.id);
        assert_eq!(tabs[0].name, "Merged");
        let panes = collect_panes(grid_or_err(&tabs[0]).expect("grid"));
        assert_eq!(
            panes.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["a1", "b1", "b2"]
        );
    }

    #[test]
    fn extract_to_new_tab_moves_panes_and_collapses_source() {
        let mut tabs = vec![make_tab_with(
            "t1",
            "A",
            split(Horizontal, 0.5, pane("p1", Some("s1")), pane("p2", None)),
        )];
        let (new_tab, source_empty) = extract_to_new_tab(
            &mut tabs,
            "t1",
            &["p1".to_string()],
            Some("Extracted".to_string()),
        )
        .expect("extract");
        assert!(!source_empty);
        assert_eq!(tabs.len(), 2);
        let source = tabs.iter().find(|t| t.id == "t1").expect("source");
        assert!(
            matches!(grid_or_err(source).expect("src grid"), GridNode::Pane { pane_id, .. } if pane_id == "p2")
        );
        assert!(
            matches!(grid_or_err(&new_tab).expect("new grid"), GridNode::Pane { pane_id, .. } if pane_id == "p1")
        );
    }

    #[test]
    fn extract_to_new_tab_empties_source_when_all_panes_moved() {
        let mut tabs = vec![make_tab_with("t1", "A", pane("only", Some("s1")))];
        let (_, source_empty) =
            extract_to_new_tab(&mut tabs, "t1", &["only".to_string()], None).expect("extract");
        assert!(source_empty);
        assert_eq!(tabs.len(), 1);
        assert_ne!(tabs[0].id, "t1");
    }

    #[test]
    fn reorder_tabs_applies_permutation() {
        let mut tabs = vec![
            make_tab_with("a", "A", pane("p1", None)),
            make_tab_with("b", "B", pane("p2", None)),
            make_tab_with("c", "C", pane("p3", None)),
        ];
        reorder_tabs(
            &mut tabs,
            &["c".to_string(), "a".to_string(), "b".to_string()],
        )
        .expect("reorder");
        assert_eq!(
            tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[test]
    fn auto_grid_cols_matches_ceil_sqrt() {
        assert_eq!(auto_grid_cols(0), 1);
        assert_eq!(auto_grid_cols(1), 1);
        assert_eq!(auto_grid_cols(2), 2);
        assert_eq!(auto_grid_cols(3), 2);
        assert_eq!(auto_grid_cols(4), 2);
        assert_eq!(auto_grid_cols(5), 3);
        assert_eq!(auto_grid_cols(6), 3);
        assert_eq!(auto_grid_cols(9), 3);
        assert_eq!(auto_grid_cols(10), 4);
    }

    #[test]
    fn build_grid_six_panes_three_cols_two_rows() {
        let panes: Vec<(String, Option<String>)> = (0..6)
            .map(|i| (format!("p{i}"), Some(format!("s{i}"))))
            .collect();
        let tree = build_grid(&panes, 3).expect("non-empty");
        // Top-level: vertical split between row1 and row2.
        let GridNode::Split {
            direction,
            first,
            second,
            ..
        } = tree
        else {
            panic!("expected vertical Split at root");
        };
        assert_eq!(direction, Vertical);
        // Each row is a balanced horizontal split of three panes.
        let row_panes = collect_panes(&first);
        assert_eq!(
            row_panes.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["p0", "p1", "p2"]
        );
        let row2_panes = collect_panes(&second);
        assert_eq!(
            row2_panes
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            vec!["p3", "p4", "p5"]
        );
        // First row's top-level split is horizontal.
        let GridNode::Split {
            direction: row_dir, ..
        } = &*first
        else {
            panic!("expected row to be a Split");
        };
        assert_eq!(*row_dir, Horizontal);
    }

    #[test]
    fn build_grid_short_last_row() {
        let panes: Vec<(String, Option<String>)> = (0..5)
            .map(|i| (format!("p{i}"), None))
            .collect();
        let tree = build_grid(&panes, 3).expect("non-empty");
        // 5 panes / 3 cols = 2 rows; row 1 = [p0,p1,p2], row 2 = [p3,p4].
        let all: Vec<String> = collect_panes(&tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(all, vec!["p0", "p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn build_grid_auto_picks_cols_when_zero() {
        let panes: Vec<(String, Option<String>)> = (0..9)
            .map(|i| (format!("p{i}"), None))
            .collect();
        let tree = build_grid(&panes, 0).expect("non-empty");
        // ceil(sqrt(9)) = 3 → 3 rows of 3.
        let GridNode::Split { first, .. } = &tree else {
            panic!("expected Split at root");
        };
        let row1 = collect_panes(first);
        assert_eq!(row1.len(), 3);
    }

    #[test]
    fn build_grid_single_pane_returns_leaf() {
        let panes = vec![("only".to_string(), Some("s".to_string()))];
        let tree = build_grid(&panes, 0).expect("non-empty");
        assert!(matches!(tree, GridNode::Pane { pane_id, .. } if pane_id == "only"));
    }

    #[test]
    fn rearrange_grid_preserves_pane_ids() {
        let original = split(
            Horizontal,
            0.5,
            pane("p1", Some("s1")),
            split(Vertical, 0.5, pane("p2", Some("s2")), pane("p3", None)),
        );
        let rearranged =
            rearrange_grid(&original, RearrangeLayout::Grid { cols: 0 }).expect("rearrange");
        let panes_after: Vec<(String, Option<String>)> = collect_panes(&rearranged);
        // ceil(sqrt(3)) = 2 cols.
        assert_eq!(panes_after.len(), 3);
        assert_eq!(
            panes_after
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            vec!["p1", "p2", "p3"]
        );
        // Sessions still attached to the right panes.
        assert_eq!(panes_after[0].1.as_deref(), Some("s1"));
        assert_eq!(panes_after[1].1.as_deref(), Some("s2"));
        assert_eq!(panes_after[2].1, None);
    }

    #[test]
    fn rearrange_grid_horizontal_makes_single_row() {
        let original = split(
            Vertical,
            0.5,
            pane("p1", None),
            pane("p2", None),
        );
        let new = rearrange_grid(&original, RearrangeLayout::Horizontal).expect("rearrange");
        let GridNode::Split { direction, .. } = &new else {
            panic!("expected Split");
        };
        assert_eq!(*direction, Horizontal);
    }

    #[test]
    fn rearrange_grid_vertical_makes_single_column() {
        let original = split(
            Horizontal,
            0.5,
            pane("p1", None),
            pane("p2", None),
        );
        let new = rearrange_grid(&original, RearrangeLayout::Vertical).expect("rearrange");
        let GridNode::Split { direction, .. } = &new else {
            panic!("expected Split");
        };
        assert_eq!(*direction, Vertical);
    }

    #[test]
    fn reorder_tabs_rejects_mismatched_set() {
        let mut tabs = vec![
            make_tab_with("a", "A", pane("p1", None)),
            make_tab_with("b", "B", pane("p2", None)),
        ];
        assert!(reorder_tabs(&mut tabs, &["a".to_string()]).is_err());
        assert!(reorder_tabs(&mut tabs, &["a".to_string(), "z".to_string()]).is_err());
    }
}
