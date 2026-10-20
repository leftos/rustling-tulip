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
        picked_layout: None,
    }
}

pub fn restore_tab(
    tabs: &mut Vec<TabEntry>,
    tab: TabEntry,
    index: usize,
) -> anyhow::Result<TabEntry> {
    if tabs.iter().any(|existing| existing.id == tab.id) {
        return Err(anyhow!("tab already exists: {}", tab.id));
    }
    let insert_at = index.min(tabs.len());
    tabs.insert(insert_at, tab.clone());
    Ok(tab)
}

pub fn restore_tab_snapshot(tabs: &mut [TabEntry], tab: TabEntry) -> anyhow::Result<TabEntry> {
    let existing = find_tab_mut(tabs, &tab.id)?;
    *existing = tab.clone();
    Ok(tab)
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

/// Swap the session bindings of two existing panes without changing topology
/// or pane ids.
pub fn swap_pane_sessions(grid: &mut GridNode, first: &str, second: &str) -> anyhow::Result<()> {
    if first == second {
        return Ok(());
    }
    let first_session = match pane_session(grid, first) {
        PaneSessionLookup::Found(session_id) => session_id,
        PaneSessionLookup::Missing => return Err(anyhow!("pane not found: {first}")),
    };
    let second_session = match pane_session(grid, second) {
        PaneSessionLookup::Found(session_id) => session_id,
        PaneSessionLookup::Missing => return Err(anyhow!("pane not found: {second}")),
    };
    replace_pane_session(grid, first, second_session)?;
    replace_pane_session(grid, second, first_session)?;
    Ok(())
}

enum PaneSessionLookup {
    Missing,
    Found(Option<String>),
}

fn pane_session(node: &GridNode, target: &str) -> PaneSessionLookup {
    match node {
        GridNode::Pane {
            pane_id,
            session_id,
        } if pane_id == target => PaneSessionLookup::Found(session_id.clone()),
        GridNode::Pane { .. } => PaneSessionLookup::Missing,
        GridNode::Split { first, second, .. } => match pane_session(first, target) {
            PaneSessionLookup::Found(session_id) => PaneSessionLookup::Found(session_id),
            PaneSessionLookup::Missing => pane_session(second, target),
        },
    }
}

/// Collect the ids of every pane in `grid` bound to `session_id`, in
/// left-to-right order. Used by the session-removal auto-close to know which
/// panes to close.
#[must_use]
pub fn panes_for_session(grid: &GridNode, session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_panes_for_session(grid, session_id, &mut out);
    out
}

fn collect_panes_for_session(grid: &GridNode, session_id: &str, out: &mut Vec<String>) {
    match grid {
        GridNode::Pane {
            pane_id,
            session_id: sid,
        } => {
            if sid.as_deref() == Some(session_id) {
                out.push(pane_id.clone());
            }
        }
        GridNode::Split { first, second, .. } => {
            collect_panes_for_session(first, session_id, out);
            collect_panes_for_session(second, session_id, out);
        }
    }
}

/// Deep-copy a layout's tabs with fresh tab + pane ids but the same session
/// bindings and structure. Used by the first-connect chooser's "clone another
/// client's layout" — the clone attaches to the same global sessions in its own
/// independent panes.
#[must_use]
pub fn clone_tabs_fresh_ids(tabs: &[TabEntry]) -> Vec<TabEntry> {
    tabs.iter()
        .map(|t| {
            let content = match &t.content {
                TabContent::Grid { grid } => TabContent::Grid {
                    grid: clone_grid_fresh_pane_ids(grid),
                },
                // Diff tabs have no panes/sessions to re-id — copy verbatim.
                diff @ TabContent::Diff { .. } => diff.clone(),
            };
            TabEntry {
                id: new_id(),
                name: t.name.clone(),
                content,
                created_at: t.created_at,
                picked_layout: t.picked_layout,
            }
        })
        .collect()
}

fn clone_grid_fresh_pane_ids(grid: &GridNode) -> GridNode {
    match grid {
        GridNode::Pane { session_id, .. } => GridNode::Pane {
            pane_id: new_id(),
            session_id: session_id.clone(),
        },
        GridNode::Split {
            direction,
            ratio,
            first,
            second,
        } => GridNode::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(clone_grid_fresh_pane_ids(first)),
            second: Box::new(clone_grid_fresh_pane_ids(second)),
        },
    }
}

/// Result of [`close_session_panes`]: tabs that survived (to re-broadcast) and
/// the ids of tabs whose last pane closed (removed from the layout).
pub struct SessionCloseResult {
    pub updated: Vec<TabEntry>,
    pub removed_tab_ids: Vec<String>,
}

/// Close every pane bound to `session_id` across `tabs`: a closed pane's
/// sibling fills its space, and a tab emptied of its last pane is removed.
/// Mutates `tabs` in place and reports what changed so the caller can broadcast
/// scoped tab events. This is the auto-close behavior on session removal.
pub fn close_session_panes(tabs: &mut Vec<TabEntry>, session_id: &str) -> SessionCloseResult {
    let mut updated = Vec::new();
    let mut removed_tab_ids = Vec::new();
    for tab in tabs.iter_mut() {
        let Some(grid) = tab.grid_mut() else {
            continue;
        };
        let pane_ids = panes_for_session(grid, session_id);
        if pane_ids.is_empty() {
            continue;
        }
        let mut emptied = false;
        for pane_id in &pane_ids {
            match close_pane(grid, pane_id) {
                Ok(true) => {
                    emptied = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => {
                    // Ids were just collected from this grid and closed in
                    // order, so a not-found here is unexpected — log and skip.
                    tracing::warn!(?err, %pane_id, "close_session_panes: pane vanished");
                }
            }
        }
        if emptied {
            removed_tab_ids.push(tab.id.clone());
        } else {
            reapply_if_untouched(tab);
            updated.push(tab.clone());
        }
    }
    tabs.retain(|t| !removed_tab_ids.contains(&t.id));
    SessionCloseResult {
        updated,
        removed_tab_ids,
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

/// Rebind every pane bound to `old` across `tabs` to `new`, in place. Used by
/// abandoned-session resume: the fresh session takes over the slot(s) the
/// abandoned one occupied instead of orphaning into the sidebar. Topology is
/// untouched. Returns clones of the tabs whose grids changed so the caller can
/// broadcast scoped tab events.
pub fn rebind_session(tabs: &mut [TabEntry], old: &str, new: &str) -> Vec<TabEntry> {
    let mut updated = Vec::new();
    for tab in tabs.iter_mut() {
        let Some(grid) = tab.grid_mut() else {
            continue;
        };
        if rebind_grid(grid, old, new) {
            updated.push(tab.clone());
        }
    }
    updated
}

fn rebind_grid(grid: &mut GridNode, old: &str, new: &str) -> bool {
    match grid {
        GridNode::Pane { session_id, .. } => {
            if session_id.as_deref() == Some(old) {
                *session_id = Some(new.to_string());
                true
            } else {
                false
            }
        }
        GridNode::Split { first, second, .. } => {
            let a = rebind_grid(first, old, new);
            let b = rebind_grid(second, old, new);
            a || b
        }
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
    // Outer-edge variants wrap the entire root rather than nesting under
    // `dst_target`. The frontend only emits an outer variant when the
    // hovered pane actually touches that outer edge of the grid; the
    // daemon trusts that decision and ignores `dst_target` here.
    if let Some((direction, source_first)) = outer_edge_layout(edge) {
        let existing = std::mem::replace(grid, placeholder());
        let (first, second) = if source_first {
            (Box::new(source), Box::new(existing))
        } else {
            (Box::new(existing), Box::new(source))
        };
        *grid = GridNode::Split {
            direction,
            ratio: DEFAULT_RATIO,
            first,
            second,
        };
        return Ok(());
    }
    if !pane_exists(grid, dst_target) {
        return Err(anyhow!("destination pane not found: {dst_target}"));
    }
    insert_in(grid, dst_target, edge, source);
    Ok(())
}

/// Translate a `PaneDropEdge::Outer*` into the split direction + which
/// side the source goes on. Returns `None` for non-outer edges so the
/// caller falls through to the per-pane insertion path.
fn outer_edge_layout(edge: PaneDropEdge) -> Option<(SplitDirection, bool)> {
    match edge {
        PaneDropEdge::OuterLeft => Some((SplitDirection::Horizontal, true)),
        PaneDropEdge::OuterRight => Some((SplitDirection::Horizontal, false)),
        PaneDropEdge::OuterTop => Some((SplitDirection::Vertical, true)),
        PaneDropEdge::OuterBottom => Some((SplitDirection::Vertical, false)),
        _ => None,
    }
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
            PaneDropEdge::Left | PaneDropEdge::Right | PaneDropEdge::Top | PaneDropEdge::Bottom => {
                let direction = match edge {
                    PaneDropEdge::Left | PaneDropEdge::Right => SplitDirection::Horizontal,
                    PaneDropEdge::Top | PaneDropEdge::Bottom => SplitDirection::Vertical,
                    _ => unreachable!("outer arm above"),
                };
                let existing = std::mem::replace(node, placeholder());
                let (first, second) = match edge {
                    PaneDropEdge::Left | PaneDropEdge::Top => {
                        (Box::new(source), Box::new(existing))
                    }
                    _ => (Box::new(existing), Box::new(source)),
                };
                *node = GridNode::Split {
                    direction,
                    ratio: DEFAULT_RATIO,
                    first,
                    second,
                };
            }
            PaneDropEdge::OuterLeft
            | PaneDropEdge::OuterRight
            | PaneDropEdge::OuterTop
            | PaneDropEdge::OuterBottom => {
                // Caller `insert_adjacent` intercepts outer variants and
                // wraps the entire root, so this arm is structurally
                // unreachable. Bail silently rather than panicking if the
                // invariant breaks — the prior topology is preserved.
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
/// guarding. This is the fallback when the pane area's shape is unknown; see
/// [`best_fit_grid_cols`] for the aspect-aware pick.
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

/// The column count whose cells come closest to square for `n` panes in an
/// area `aspect` wide per unit of height, with each empty cell weighing 0.3
/// and a one-row or one-column strip of five or more 0.4 more; an aspect that
/// is not a positive number counts as 16:9. Ported from the Tauri client's
/// `bestFitGridCols`.
#[must_use]
pub(crate) fn best_fit_grid_cols(n: usize, aspect: f32) -> usize {
    if n <= 1 {
        return 1;
    }
    let aspect = f64::from(aspect);
    let aspect = if aspect > 0.0 && aspect.is_finite() {
        aspect
    } else {
        16.0 / 9.0
    };
    let mut best = 1;
    let mut best_score = f64::INFINITY;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let empties = rows * cols - n;
        let cell_aspect = aspect * as_f64(rows) / as_f64(cols);
        let mut score = cell_aspect.ln().abs() + 0.3 * as_f64(empties);
        if (cols == 1 || cols == n) && n >= 5 {
            score += 0.4;
        }
        if score < best_score {
            best_score = score;
            best = cols;
        }
    }
    best
}

fn as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The automatic column count for `n` panes: the aspect-aware best fit when
/// the pane area's shape is known, else [`auto_grid_cols`].
fn auto_cols(n: usize, aspect: Option<f32>) -> u32 {
    let cols = match aspect {
        Some(aspect) => best_fit_grid_cols(n, aspect),
        None => return auto_grid_cols(n),
    };
    u32::try_from(cols).unwrap_or(u32::MAX)
}

/// Build a true 2D grid: `cols` columns per row, rows stacked vertically.
/// The last row may be short. Returns `None` for an empty input.
///
/// `cols == 0` is rewritten to the automatic pick — [`best_fit_grid_cols`]
/// when `aspect` (the pane area's width over its height) is known,
/// [`auto_grid_cols`] otherwise — so callers can pass an automatic layout
/// straight through.
pub fn build_grid(
    panes: &[(String, Option<String>)],
    cols: u32,
    aspect: Option<f32>,
) -> Option<GridNode> {
    if panes.is_empty() {
        return None;
    }
    let cols = if cols == 0 {
        auto_cols(panes.len(), aspect)
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

/// Rebuild a tab's grid using `layout`, dropping any panes that aren't
/// bound to a session — rearrange is user-triggered and "tidy up" intent
/// implies the empty placeholders should go. Pane order is left-to-right
/// as walked by [`collect_panes`]. If every pane is unbound, the grid is
/// kept as-is and only rearranged (collapsing to zero panes would leave
/// the tab in an invalid state). Returns the new grid; caller is
/// responsible for writing it back to the tab and broadcasting
/// `TabUpdated`.
pub fn rearrange_grid(grid: &GridNode, layout: RearrangeLayout) -> anyhow::Result<GridNode> {
    let all_panes = collect_panes(grid);
    if all_panes.is_empty() {
        return Err(anyhow!("cannot rearrange an empty grid"));
    }
    let bound: Vec<(String, Option<String>)> = all_panes
        .iter()
        .filter(|(_, session_id)| session_id.is_some())
        .cloned()
        .collect();
    let panes = if bound.is_empty() { all_panes } else { bound };
    build_for_layout(&panes, layout)
        .ok_or_else(|| anyhow!("rearrange produced empty tab (unreachable)"))
}

/// Build a grid tree laying `panes` out per `layout`. Shared by the
/// "Rearrange panes" path and "Move panes to a new tab" so a requested
/// layout means the same shape in both. Returns `None` only for an empty
/// `panes` slice.
fn build_for_layout(
    panes: &[(String, Option<String>)],
    layout: RearrangeLayout,
) -> Option<GridNode> {
    match layout {
        RearrangeLayout::Horizontal | RearrangeLayout::Balanced => {
            build_balanced(panes, SplitDirection::Horizontal)
        }
        RearrangeLayout::Vertical => build_balanced(panes, SplitDirection::Vertical),
        RearrangeLayout::Grid { cols, aspect } => build_grid(panes, cols, aspect),
        RearrangeLayout::Unknown => {
            // Forward-compat: client requested a layout this daemon doesn't
            // know. Fall back to Balanced (the safest known shape — keeps
            // every pane visible without strong orientation claims).
            tracing::warn!("layout: unknown variant from client; falling back to balanced");
            build_balanced(panes, SplitDirection::Horizontal)
        }
    }
}

/// The layout [`build_for_layout`] actually builds for `layout`: a variant this
/// build does not know falls back to `Balanced`, and that is the type a later
/// rebuild re-applies.
const fn built_layout(layout: RearrangeLayout) -> RearrangeLayout {
    match layout {
        RearrangeLayout::Unknown => RearrangeLayout::Balanced,
        other => other,
    }
}

/// Whether `layout` is an automatic tab's re-fit: the tab is already automatic
/// and the request names no column count either, so the only thing that
/// changed is the pane area's shape. A fresh automatic pick (`picked` empty or
/// naming something else) is not a re-fit, and neither is any fixed layout.
fn is_auto_refit(picked: Option<RearrangeLayout>, layout: RearrangeLayout) -> bool {
    matches!(picked, Some(RearrangeLayout::Grid { cols: 0, .. }))
        && matches!(layout, RearrangeLayout::Grid { cols: 0, .. })
}

/// Arrange `tab` per `layout` as a layout picker does (Rearrange, the
/// first-connect chooser) and mark it untouched since that pick, so later pane
/// additions and removals rebuild the same layout type. The remembered type is
/// the one actually built, so an unknown wire layout is remembered as
/// `Balanced` rather than falling back again on every rebuild.
///
/// An automatic tab's re-fit — the client re-sending the automatic grid with
/// the pane area's new shape after a resize — rebuilds over every pane it
/// holds, empty ones included, since nobody asked for a tidy-up; every other
/// pick drops the unbound panes as a tidy-up does.
pub fn pick_layout(tab: &mut TabEntry, layout: RearrangeLayout) -> anyhow::Result<()> {
    let refit = is_auto_refit(tab.picked_layout, layout);
    let grid = grid_or_err_mut(tab)?;
    if refit {
        *grid = build_for_layout(&collect_panes(grid), layout)
            .ok_or_else(|| anyhow!("re-fit produced an empty tab (unreachable)"))?;
    } else {
        *grid = rearrange_grid(grid, layout)?;
    }
    tab.picked_layout = Some(built_layout(layout));
    Ok(())
}

/// The user arranged `tab`'s panes by hand (a divider drag, a move within the
/// tab): from now on adding or removing a pane keeps the sizes it has.
pub fn touch(tab: &mut TabEntry) {
    tab.picked_layout = None;
}

/// Rebuild an untouched tab's picked layout over the panes it holds now, keeping
/// their ids, their walk order and the empty ones. Returns true when the tab
/// was rebuilt; a tab touched since its pick, or never picked, is left alone.
pub fn reapply_if_untouched(tab: &mut TabEntry) -> bool {
    let Some(layout) = tab.picked_layout else {
        return false;
    };
    let Some(grid) = tab.grid_mut() else {
        return false;
    };
    let Some(rebuilt) = build_for_layout(&collect_panes(grid), layout) else {
        return false;
    };
    *grid = rebuilt;
    true
}

/// [`split_pane`] on `tab`'s grid, then rebuild the tab's layout when it is
/// untouched since picked. Returns the new pane's id.
pub fn split_tab_pane(
    tab: &mut TabEntry,
    target: &str,
    direction: SplitDirection,
    place: SplitPlace,
    new_session_id: Option<String>,
) -> anyhow::Result<String> {
    let new_pane_id = split_pane(
        grid_or_err_mut(tab)?,
        target,
        direction,
        place,
        new_session_id,
    )?;
    reapply_if_untouched(tab);
    Ok(new_pane_id)
}

/// [`close_pane`] on `tab`'s grid, then rebuild the surviving panes' layout
/// when the tab is untouched since picked. Returns `Ok(true)` when the close
/// emptied the tab (caller removes it).
pub fn close_tab_pane(tab: &mut TabEntry, target: &str) -> anyhow::Result<bool> {
    let emptied = close_pane(grid_or_err_mut(tab)?, target)?;
    if !emptied {
        reapply_if_untouched(tab);
    }
    Ok(emptied)
}

/// [`set_pane_ratio`] on `tab`'s grid: a divider drag, so the tab is touched.
pub fn set_tab_pane_ratio(tab: &mut TabEntry, split_path: &[u8], ratio: f32) -> anyhow::Result<()> {
    set_pane_ratio(grid_or_err_mut(tab)?, split_path, ratio)?;
    touch(tab);
    Ok(())
}

/// The tab a [`close_pane_in_tabs`] changed.
#[derive(Debug, Clone, PartialEq)]
pub enum ClosedTab {
    Updated(TabEntry),
    Removed(String),
}

/// Close `pane_id` in `tab_id`: the tab is rebuilt when it is untouched since a
/// pick, and removed when that was its last pane. Caller broadcasts the outcome.
pub fn close_pane_in_tabs(
    tabs: &mut Vec<TabEntry>,
    tab_id: &str,
    pane_id: &str,
) -> anyhow::Result<ClosedTab> {
    let tab = find_tab_mut(tabs, tab_id)?;
    let emptied = close_tab_pane(tab, pane_id)?;
    if emptied {
        tabs.retain(|t| t.id != tab_id);
        Ok(ClosedTab::Removed(tab_id.to_string()))
    } else {
        Ok(ClosedTab::Updated(tab.clone()))
    }
}

/// One pane move: the pane `src_pane_id` of `src_tab_id` dropped on `edge` of
/// `dst_pane_id` in `dst_tab_id` (the same tab or another).
pub struct PaneMove<'a> {
    pub src_tab_id: &'a str,
    pub src_pane_id: &'a str,
    pub dst_tab_id: &'a str,
    pub dst_pane_id: &'a str,
    pub edge: PaneDropEdge,
}

/// A tab a [`move_pane`] changed, in the order the caller broadcasts them.
#[derive(Debug, Clone, PartialEq)]
pub enum MovedTab {
    Updated(TabEntry),
    Removed(String),
}

/// Move a pane within a tab or across tabs. A move within one tab, a swap
/// included, is a hand arrangement and touches the tab; a move across tabs
/// rebuilds each untouched side over its new pane count. On error `tabs` may be
/// half-changed, so the caller runs this inside a mutation that rolls back.
pub fn move_pane(tabs: &mut Vec<TabEntry>, mv: &PaneMove<'_>) -> anyhow::Result<Vec<MovedTab>> {
    if mv.src_tab_id == mv.dst_tab_id {
        move_within_tab(tabs, mv)
    } else {
        move_across_tabs(tabs, mv)
    }
}

fn move_within_tab(tabs: &mut [TabEntry], mv: &PaneMove<'_>) -> anyhow::Result<Vec<MovedTab>> {
    let tab = find_tab_mut(tabs, mv.src_tab_id)?;
    let grid = grid_or_err_mut(tab)?;
    if mv.edge == PaneDropEdge::Replace {
        swap_pane_sessions(grid, mv.src_pane_id, mv.dst_pane_id)?;
    } else {
        let (extracted, source_empty) = extract_pane(grid, mv.src_pane_id)?;
        if source_empty {
            return Err(anyhow!("cannot move the only pane within the same tab"));
        }
        insert_adjacent(grid, mv.dst_pane_id, mv.edge, extracted)?;
    }
    touch(tab);
    Ok(vec![MovedTab::Updated(tab.clone())])
}

fn move_across_tabs(tabs: &mut Vec<TabEntry>, mv: &PaneMove<'_>) -> anyhow::Result<Vec<MovedTab>> {
    let src_tab = find_tab_mut(tabs, mv.src_tab_id)?;
    let (extracted, source_empty) = extract_pane(grid_or_err_mut(src_tab)?, mv.src_pane_id)?;
    let mut moved = Vec::new();
    if source_empty {
        tabs.retain(|t| t.id != mv.src_tab_id);
        moved.push(MovedTab::Removed(mv.src_tab_id.to_string()));
    } else {
        reapply_if_untouched(src_tab);
        moved.push(MovedTab::Updated(src_tab.clone()));
    }
    let dst_tab = find_tab_mut(tabs, mv.dst_tab_id)?;
    insert_adjacent(
        grid_or_err_mut(dst_tab)?,
        mv.dst_pane_id,
        mv.edge,
        extracted,
    )?;
    reapply_if_untouched(dst_tab);
    moved.push(MovedTab::Updated(dst_tab.clone()));
    Ok(moved)
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
        picked_layout: None,
    };
    tabs.push(entry.clone());
    Ok(entry)
}

/// What [`extract_to_new_tab`] changed.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractOutcome {
    /// The fresh tab holding the extracted panes.
    pub new_tab: TabEntry,
    /// The source tab as it survives, or `None` when the extraction emptied
    /// and removed it.
    pub source: Option<TabEntry>,
}

/// Move a set of panes out of `source_tab_id` into a fresh tab. Returns the
/// new tab and the surviving source tab. `layout` controls how the new tab
/// arranges the extracted panes and is a pick: the new tab is untouched since
/// it. `None` keeps the historical horizontal tiling and picks nothing. A
/// surviving source tab untouched since its own pick is rebuilt over the panes
/// left. If extracting empties the source tab, the source is removed (caller
/// must broadcast `TabRemoved` for the source as well).
pub fn extract_to_new_tab(
    tabs: &mut Vec<TabEntry>,
    source_tab_id: &str,
    pane_ids: &[String],
    name: Option<String>,
    layout: Option<RearrangeLayout>,
) -> anyhow::Result<ExtractOutcome> {
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
    } else {
        reapply_if_untouched(source);
    }
    let source = if source_empty {
        None
    } else {
        tabs.iter().find(|t| t.id == source_tab_id).cloned()
    };
    let grid = build_for_layout(&extracted, layout.unwrap_or(RearrangeLayout::Horizontal))
        .ok_or_else(|| anyhow!("extract produced empty tab"))?;
    let resolved_name = name.unwrap_or_else(|| next_default_name(tabs));
    let entry = TabEntry {
        id: new_id(),
        name: resolved_name,
        content: TabContent::Grid { grid },
        created_at: Utc::now(),
        picked_layout: layout.map(built_layout),
    };
    tabs.push(entry.clone());
    Ok(ExtractOutcome {
        new_tab: entry,
        source,
    })
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
    fn restore_tab_reinserts_snapshot_at_index() {
        let mut tabs = vec![
            make_tab(Some("A".to_string()), None, &[]),
            make_tab(Some("B".to_string()), None, &[]),
        ];
        let restored = TabEntry {
            id: "restored".to_string(),
            name: "Restored".to_string(),
            content: TabContent::Grid {
                grid: pane("p-restored", Some("s-restored")),
            },
            created_at: Utc::now(),
            picked_layout: None,
        };

        let inserted = restore_tab(&mut tabs, restored.clone(), 1).expect("restore");

        assert_eq!(inserted.id, "restored");
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[1].id, "restored");
        assert_eq!(tabs[1], restored);
    }

    #[test]
    fn restore_tab_rejects_duplicate_id() {
        let existing = make_tab(Some("A".to_string()), None, &[]);
        let mut tabs = vec![existing.clone()];
        let err = restore_tab(&mut tabs, existing, 0).expect_err("duplicate id");

        assert!(err.to_string().contains("tab already exists"));
    }

    #[test]
    fn restore_tab_snapshot_replaces_live_tab() {
        let original = make_tab(Some("Original".to_string()), None, &[]);
        let mut tabs = vec![original.clone()];
        let replacement = TabEntry {
            id: original.id.clone(),
            name: "Restored".to_string(),
            content: TabContent::Grid {
                grid: pane("restored-pane", None),
            },
            created_at: original.created_at,
            picked_layout: None,
        };

        let restored =
            restore_tab_snapshot(&mut tabs, replacement.clone()).expect("restore snapshot");

        assert_eq!(restored, replacement);
        assert_eq!(tabs[0], replacement);
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

    fn grid_tab(id: &str, grid: GridNode) -> TabEntry {
        TabEntry {
            id: id.to_string(),
            name: id.to_string(),
            content: TabContent::Grid { grid },
            created_at: Utc::now(),
            picked_layout: None,
        }
    }

    /// An `AppState` over a throwaway config dir, with its `Dirs` so the test
    /// can clean the directory up at the end.
    fn scratch_state(tag: &str) -> (crate::state::AppState, crate::paths::Dirs) {
        let root = std::env::temp_dir().join(format!("rt-tabs-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        let dirs = crate::paths::Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            remote_config_file: root.join("remote.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
        };
        let state = crate::state::AppState::load_or_default(&dirs).expect("load state");
        (state, dirs)
    }

    #[test]
    fn panes_for_session_collects_matching_pane_ids() {
        let grid = split(
            Horizontal,
            0.5,
            pane("p1", Some("s")),
            split(
                Vertical,
                0.5,
                pane("p2", Some("s")),
                pane("p3", Some("other")),
            ),
        );
        let ids = panes_for_session(&grid, "s");
        assert_eq!(ids, vec!["p1".to_string(), "p2".to_string()]);
        assert_eq!(
            panes_for_session(&grid, "absent"),
            [] as [std::string::String; 0]
        );
    }

    #[test]
    fn close_session_panes_fills_siblings_and_removes_empty_tabs() {
        // tab A: single pane on the doomed session -> tab is removed.
        // tab B: two panes, only one on the session -> that pane closes, the
        //        sibling fills, the tab survives.
        // tab C: no panes on the session -> untouched, not reported.
        let mut tabs = vec![
            grid_tab("A", pane("a1", Some("doomed"))),
            grid_tab(
                "B",
                split(
                    Horizontal,
                    0.5,
                    pane("b1", Some("doomed")),
                    pane("b2", Some("keep")),
                ),
            ),
            grid_tab("C", pane("c1", Some("keep"))),
        ];

        let result = close_session_panes(&mut tabs, "doomed");

        // Tab A removed; B updated; C untouched.
        assert_eq!(result.removed_tab_ids, vec!["A".to_string()]);
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].id, "B");
        // A is gone from the vec; B + C remain.
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, "B");
        assert_eq!(tabs[1].id, "C");
        // B collapsed to just the surviving pane.
        let b_panes = collect_panes(tabs[0].grid().expect("B has a grid"));
        assert_eq!(b_panes, vec![("b2".to_string(), Some("keep".to_string()))]);
    }

    #[test]
    fn close_session_panes_removes_tab_when_all_panes_share_the_session() {
        let mut tabs = vec![grid_tab(
            "T",
            split(
                Horizontal,
                0.5,
                pane("t1", Some("s")),
                pane("t2", Some("s")),
            ),
        )];
        let result = close_session_panes(&mut tabs, "s");
        assert_eq!(result.removed_tab_ids, vec!["T".to_string()]);
        assert_eq!(result.updated, [] as [protocol::TabEntry; 0]);
        assert!(
            tabs.is_empty(),
            "tab with all panes on the session is removed"
        );
    }

    #[test]
    fn rebind_session_rewrites_matching_panes_across_tabs() {
        // tab A: single pane on the old session.
        // tab B: two panes, one on old + one on a different session.
        // tab C: no panes on old -> untouched, not reported.
        let mut tabs = vec![
            grid_tab("A", pane("a1", Some("old"))),
            grid_tab(
                "B",
                split(
                    Horizontal,
                    0.5,
                    pane("b1", Some("old")),
                    pane("b2", Some("other")),
                ),
            ),
            grid_tab("C", pane("c1", Some("other"))),
        ];

        let updated = rebind_session(&mut tabs, "old", "new");

        // A + B reported (changed); C not.
        let updated_ids: Vec<String> = updated.iter().map(|t| t.id.clone()).collect();
        assert_eq!(updated_ids, vec!["A".to_string(), "B".to_string()]);

        // Panes bound to old now point at new; other bindings + topology intact.
        assert_eq!(
            collect_panes(tabs[0].grid().expect("A grid")),
            vec![("a1".to_string(), Some("new".to_string()))]
        );
        assert_eq!(
            collect_panes(tabs[1].grid().expect("B grid")),
            vec![
                ("b1".to_string(), Some("new".to_string())),
                ("b2".to_string(), Some("other".to_string())),
            ]
        );
        assert_eq!(
            collect_panes(tabs[2].grid().expect("C grid")),
            vec![("c1".to_string(), Some("other".to_string()))]
        );
    }

    #[test]
    fn clone_tabs_fresh_ids_keeps_sessions_but_remints_ids() {
        let src = vec![grid_tab(
            "orig",
            split(
                Vertical,
                0.5,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )];
        let cloned = clone_tabs_fresh_ids(&src);
        assert_eq!(cloned.len(), 1);
        assert_ne!(cloned[0].id, "orig", "tab id is fresh");
        let panes = collect_panes(cloned[0].grid().expect("cloned grid"));
        let sessions: Vec<_> = panes.iter().map(|(_, sid)| sid.clone()).collect();
        assert_eq!(
            sessions,
            vec![Some("s1".to_string()), Some("s2".to_string())],
            "same session bindings"
        );
        let pane_ids: Vec<_> = panes.iter().map(|(pid, _)| pid.clone()).collect();
        assert!(!pane_ids.contains(&"p1".to_string()), "pane ids are fresh");
        assert!(!pane_ids.contains(&"p2".to_string()), "pane ids are fresh");
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
    fn insert_adjacent_outer_right_wraps_entire_root() {
        // Start with a 2-column row. Dropping on the outer-right band of
        // the right pane should wrap the whole row, not nest under that
        // pane — so the source ends up as a full-height column on the
        // right at the topmost split level.
        let mut grid = split(
            Horizontal,
            0.5,
            pane("a", Some("sa")),
            pane("b", Some("sb")),
        );
        insert_adjacent(
            &mut grid,
            "b",
            PaneDropEdge::OuterRight,
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
            panic!("expected outer Split at root");
        };
        assert_eq!(direction, Horizontal);
        // Existing 2-col row is on the left side intact.
        assert!(matches!(
            &*first,
            GridNode::Split {
                direction: Horizontal,
                ..
            }
        ));
        // Source pane is the full-height right column.
        assert!(matches!(&*second, GridNode::Pane { pane_id, .. } if pane_id == "src"));
    }

    #[test]
    fn insert_adjacent_outer_top_wraps_root_into_vertical_split() {
        let mut grid = pane("dst", Some("sd"));
        insert_adjacent(
            &mut grid,
            "dst",
            PaneDropEdge::OuterTop,
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
        assert_eq!(direction, Vertical);
        assert!(matches!(&*first, GridNode::Pane { pane_id, .. } if pane_id == "src"));
        assert!(matches!(&*second, GridNode::Pane { pane_id, .. } if pane_id == "dst"));
    }

    #[test]
    fn insert_adjacent_outer_ignores_missing_dst_pane() {
        // Outer-edge variants don't depend on `dst_target` — the source
        // wraps the entire root regardless of what pane id the caller
        // passes. The frontend always picks a real pane id, but the
        // daemon shouldn't fail the move if it's stale.
        let mut grid = pane("real", Some("sr"));
        insert_adjacent(
            &mut grid,
            "does-not-exist",
            PaneDropEdge::OuterLeft,
            pane("src", Some("ss")),
        )
        .expect("insert");
        assert!(matches!(
            &grid,
            GridNode::Split {
                direction: Horizontal,
                ..
            }
        ));
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
    fn swap_pane_sessions_preserves_topology_and_pane_ids() {
        let mut grid = split(
            Horizontal,
            0.5,
            pane("left", Some("s-left")),
            split(
                Vertical,
                0.5,
                pane("top-right", Some("s-top")),
                pane("bottom-right", Some("s-bottom")),
            ),
        );
        swap_pane_sessions(&mut grid, "left", "bottom-right").expect("swap");
        assert!(matches!(&grid, GridNode::Split { first, second, .. }
            if matches!(&**first, GridNode::Pane { pane_id, session_id }
                if pane_id == "left" && session_id.as_deref() == Some("s-bottom"))
            && matches!(&**second, GridNode::Split { first, second, .. }
                if matches!(&**first, GridNode::Pane { pane_id, session_id }
                    if pane_id == "top-right" && session_id.as_deref() == Some("s-top"))
                && matches!(&**second, GridNode::Pane { pane_id, session_id }
                    if pane_id == "bottom-right" && session_id.as_deref() == Some("s-left")))));
    }

    #[test]
    fn swap_pane_sessions_reports_missing_pane() {
        let mut grid = pane("only", Some("s1"));
        let err = swap_pane_sessions(&mut grid, "only", "missing").expect_err("missing pane");
        assert!(err.to_string().contains("pane not found: missing"));
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
            picked_layout: None,
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
        let outcome = extract_to_new_tab(
            &mut tabs,
            "t1",
            &["p1".to_string()],
            Some("Extracted".to_string()),
            None,
        )
        .expect("extract");
        assert!(outcome.source.is_some());
        let new_tab = outcome.new_tab;
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
        let outcome = extract_to_new_tab(&mut tabs, "t1", &["only".to_string()], None, None)
            .expect("extract");
        assert!(outcome.source.is_none());
        assert_eq!(tabs.len(), 1);
        assert_ne!(tabs[0].id, "t1");
    }

    /// The extract pulls panes out of the source grid one at a time, so a pane
    /// id that names nothing fails once the earlier ones are already gone.
    /// Through the state the server mutates, that failure has to roll the whole
    /// layout back instead of persisting a half-drained tab.
    #[test]
    fn failed_extract_to_new_tab_leaves_layout_unchanged() {
        let (state, dirs) = scratch_state("rollback-extract");
        let grid = split(Horizontal, 0.5, pane("p1", None), pane("p2", None));
        state
            .mutate_client_layout("c1", |tabs| tabs.push(grid_tab("t1", grid)))
            .expect("seed the client layout");
        let before = state.client_layout("c1");

        let err = state
            .try_mutate_client_layout("c1", |tabs| {
                extract_to_new_tab(
                    tabs,
                    "t1",
                    &["p1".to_string(), "ghost".to_string()],
                    None,
                    None,
                )
            })
            .expect_err("the second pane id names no pane");
        assert!(err.to_string().contains("ghost"), "unexpected error: {err}");

        assert_eq!(
            state.client_layout("c1"),
            before,
            "a failed extract leaves the tab list exactly as it was"
        );

        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn extract_to_new_tab_honors_grid_layout() {
        // Source tab is a flat horizontal strip of six bound panes p0..p5.
        // Move the first four into a new tab arranged as a 2-column grid; the
        // new tab's root must be a vertical split (rows) whose first row
        // carries p0 and p1.
        let mut grid = pane("p0", Some("s0"));
        for i in 1..6 {
            let pid = format!("p{i}");
            let sid = format!("s{i}");
            grid = split(Horizontal, 0.5, grid, pane(&pid, Some(sid.as_str())));
        }
        let mut tabs = vec![make_tab_with("t1", "A", grid)];
        let moved: Vec<String> = (0..4).map(|i| format!("p{i}")).collect();
        let outcome = extract_to_new_tab(
            &mut tabs,
            "t1",
            &moved,
            None,
            Some(RearrangeLayout::Grid {
                cols: 2,
                aspect: None,
            }),
        )
        .expect("extract");
        assert!(outcome.source.is_some());
        let new_tab = outcome.new_tab;
        let new_grid = grid_or_err(&new_tab).expect("new grid");
        let GridNode::Split {
            direction, first, ..
        } = new_grid
        else {
            panic!("expected vertical Split at grid root");
        };
        assert_eq!(*direction, Vertical);
        let first_row: Vec<String> = collect_panes(first).into_iter().map(|(p, _)| p).collect();
        assert_eq!(first_row, vec!["p0".to_string(), "p1".to_string()]);
        let all: Vec<String> = collect_panes(new_grid)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(all, vec!["p0", "p1", "p2", "p3"]);
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

    /// Worked by hand from the Tauri client's score
    /// `|ln(aspect × rows / cols)| + 0.3 × empties (+ 0.4 for a strip at 5+)`:
    /// - 4 at 16:9: 2 cols scores 0.575, 3 cols 0.770, 4 cols 0.811 → 2.
    /// - 9 at 16:9: 3 cols 0.575, 5 cols 0.641, 4 cols 1.188 → 3.
    /// - 5 at 16:10: 3 cols 0.365, 4 cols 1.123, 2 cols 1.175 → 3.
    /// - 3 at 1:2: 1 col 0.405, 2 cols 0.993, 3 cols 1.792 → 1.
    /// - an aspect that is not positive or not finite falls back to 16:9,
    ///   so 4 → 2.
    #[test]
    fn best_fit_grid_cols_matches_tauri_cases() {
        assert_eq!(best_fit_grid_cols(0, 16.0 / 9.0), 1);
        assert_eq!(best_fit_grid_cols(1, 16.0 / 9.0), 1);
        assert_eq!(best_fit_grid_cols(4, 16.0 / 9.0), 2);
        assert_eq!(best_fit_grid_cols(9, 16.0 / 9.0), 3);
        assert_eq!(best_fit_grid_cols(5, 16.0 / 10.0), 3);
        assert_eq!(best_fit_grid_cols(3, 0.5), 1);
        assert_eq!(best_fit_grid_cols(4, 0.0), 2);
        assert_eq!(best_fit_grid_cols(4, -3.0), 2);
        assert_eq!(best_fit_grid_cols(4, f32::NAN), 2);
        assert_eq!(best_fit_grid_cols(4, f32::INFINITY), 2);
    }

    #[test]
    fn build_grid_six_panes_three_cols_two_rows() {
        let panes: Vec<(String, Option<String>)> = (0..6)
            .map(|i| (format!("p{i}"), Some(format!("s{i}"))))
            .collect();
        let tree = build_grid(&panes, 3, None).expect("non-empty");
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
            row_panes
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
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
        let panes: Vec<(String, Option<String>)> =
            (0..5).map(|i| (format!("p{i}"), None)).collect();
        let tree = build_grid(&panes, 3, None).expect("non-empty");
        // 5 panes / 3 cols = 2 rows; row 1 = [p0,p1,p2], row 2 = [p3,p4].
        let all: Vec<String> = collect_panes(&tree).into_iter().map(|(p, _)| p).collect();
        assert_eq!(all, vec!["p0", "p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn build_grid_auto_picks_cols_when_zero() {
        let panes: Vec<(String, Option<String>)> =
            (0..9).map(|i| (format!("p{i}"), None)).collect();
        let tree = build_grid(&panes, 0, None).expect("non-empty");
        // ceil(sqrt(9)) = 3 → 3 rows of 3.
        let GridNode::Split { first, .. } = &tree else {
            panic!("expected Split at root");
        };
        let row1 = collect_panes(first);
        assert_eq!(row1.len(), 3);
    }

    #[test]
    fn build_grid_auto_uses_the_aspect_when_given() {
        let panes: Vec<(String, Option<String>)> =
            (0..3).map(|i| (format!("p{i}"), None)).collect();
        let tall = build_grid(&panes, 0, Some(0.5)).expect("non-empty");
        let square = build_grid(&panes, 0, None).expect("non-empty");
        assert_eq!(
            tall,
            build_grid(&panes, 1, None).expect("non-empty"),
            "three panes in an area half as wide as tall fit one column"
        );
        assert_eq!(
            square,
            build_grid(&panes, 2, None).expect("non-empty"),
            "with no aspect it is ceil(sqrt(3)) = two columns"
        );
        assert_ne!(tall, square, "the aspect decides the auto pick");
    }

    #[test]
    fn build_grid_single_pane_returns_leaf() {
        let panes = vec![("only".to_string(), Some("s".to_string()))];
        let tree = build_grid(&panes, 0, None).expect("non-empty");
        assert!(matches!(tree, GridNode::Pane { pane_id, .. } if pane_id == "only"));
    }

    #[test]
    fn rearrange_grid_drops_unbound_panes() {
        // Mixed grid: two session-bound panes and one empty placeholder.
        // Rearrange is user-triggered "tidy up", so the empty pane should
        // be dropped and only the bound panes survive.
        let original = split(
            Horizontal,
            0.5,
            pane("p1", Some("s1")),
            split(Vertical, 0.5, pane("p2", Some("s2")), pane("p3", None)),
        );
        let rearranged = rearrange_grid(
            &original,
            RearrangeLayout::Grid {
                cols: 0,
                aspect: None,
            },
        )
        .expect("rearrange");
        let panes_after: Vec<(String, Option<String>)> = collect_panes(&rearranged);
        assert_eq!(panes_after.len(), 2);
        assert_eq!(panes_after[0].0, "p1");
        assert_eq!(panes_after[0].1.as_deref(), Some("s1"));
        assert_eq!(panes_after[1].0, "p2");
        assert_eq!(panes_after[1].1.as_deref(), Some("s2"));
    }

    #[test]
    fn rearrange_grid_keeps_all_when_no_panes_bound() {
        // Edge case: every pane is empty. Dropping them all would leave the
        // tab with zero panes (no valid representation), so we fall back to
        // rearranging the original set in place.
        let original = split(Horizontal, 0.5, pane("p1", None), pane("p2", None));
        let rearranged = rearrange_grid(
            &original,
            RearrangeLayout::Grid {
                cols: 0,
                aspect: None,
            },
        )
        .expect("rearrange");
        let panes_after = collect_panes(&rearranged);
        assert_eq!(panes_after.len(), 2);
        assert_eq!(panes_after[0].0, "p1");
        assert_eq!(panes_after[1].0, "p2");
    }

    #[test]
    fn rearrange_grid_horizontal_makes_single_row() {
        let original = split(Vertical, 0.5, pane("p1", None), pane("p2", None));
        let new = rearrange_grid(&original, RearrangeLayout::Horizontal).expect("rearrange");
        let GridNode::Split { direction, .. } = &new else {
            panic!("expected Split");
        };
        assert_eq!(*direction, Horizontal);
    }

    #[test]
    fn rearrange_grid_vertical_makes_single_column() {
        let original = split(Horizontal, 0.5, pane("p1", None), pane("p2", None));
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

    /// Panes `ids`, each bound to the session `s-<id>`.
    fn bound(ids: &[&str]) -> Vec<(String, Option<String>)> {
        ids.iter()
            .map(|id| ((*id).to_string(), Some(format!("s-{id}"))))
            .collect()
    }

    /// A tab laid out per `layout` over `panes` and marked untouched since
    /// that pick.
    fn picked_tab(
        id: &str,
        panes: &[(String, Option<String>)],
        layout: RearrangeLayout,
    ) -> TabEntry {
        let grid = build_for_layout(panes, layout).expect("non-empty panes");
        TabEntry {
            picked_layout: Some(layout),
            ..grid_tab(id, grid)
        }
    }

    fn grid_of(tab: &TabEntry) -> &GridNode {
        tab.grid().expect("a grid tab")
    }

    #[test]
    fn pick_layout_marks_tab_untouched_and_remembers_the_type() {
        let mut tab = grid_tab(
            "T",
            split(
                Vertical,
                0.3,
                pane("a", Some("s-a")),
                pane("b", Some("s-b")),
            ),
        );
        pick_layout(&mut tab, RearrangeLayout::Horizontal).expect("pick");
        assert_eq!(tab.picked_layout, Some(RearrangeLayout::Horizontal));
        assert_eq!(
            grid_of(&tab),
            &build_balanced(&bound(&["a", "b"]), Horizontal).expect("grid")
        );
    }

    /// An unknown wire layout is built as `Balanced`, so that fallback — not
    /// `Unknown` — is the type later rebuilds re-apply.
    #[test]
    fn pick_layout_with_an_unknown_layout_remembers_the_built_layout() {
        let mut tab = grid_tab(
            "T",
            split(
                Vertical,
                0.3,
                pane("a", Some("s-a")),
                pane("b", Some("s-b")),
            ),
        );
        pick_layout(&mut tab, RearrangeLayout::Unknown).expect("pick");
        assert_eq!(tab.picked_layout, Some(RearrangeLayout::Balanced));
        assert_eq!(
            grid_of(&tab),
            &build_balanced(&bound(&["a", "b"]), Horizontal).expect("grid")
        );
    }

    #[test]
    fn split_pane_on_an_untouched_horizontal_tab_stays_evenly_balanced() {
        let mut tab = picked_tab("T", &bound(&["a", "b"]), RearrangeLayout::Horizontal);
        let new_id = split_tab_pane(
            &mut tab,
            "b",
            Vertical,
            SplitPlace::Second,
            Some("s-c".to_string()),
        )
        .expect("split");
        let mut panes = bound(&["a", "b"]);
        panes.push((new_id, Some("s-c".to_string())));
        assert_eq!(
            grid_of(&tab),
            &build_balanced(&panes, Horizontal).expect("grid")
        );
        assert_eq!(tab.picked_layout, Some(RearrangeLayout::Horizontal));
    }

    #[test]
    fn split_pane_on_an_untouched_three_column_grid_makes_three_plus_one() {
        let three_cols = RearrangeLayout::Grid {
            cols: 3,
            aspect: None,
        };
        let mut tab = picked_tab("T", &bound(&["a", "b", "c"]), three_cols);
        let new_id =
            split_tab_pane(&mut tab, "c", Horizontal, SplitPlace::Second, None).expect("split");
        let mut panes = bound(&["a", "b", "c"]);
        panes.push((new_id, None));
        let three_plus_one = build_grid(&panes, 3, None).expect("grid");
        assert_ne!(
            three_plus_one,
            build_grid(&panes, 0, None).expect("grid"),
            "four panes auto-pick two columns, so the kept count is visible"
        );
        assert_eq!(grid_of(&tab), &three_plus_one);
        assert_eq!(tab.picked_layout, Some(three_cols));
    }

    #[test]
    fn split_pane_on_an_untouched_auto_grid_re_picks_the_columns() {
        let auto = RearrangeLayout::Grid {
            cols: 0,
            aspect: None,
        };
        let mut tab = picked_tab("T", &bound(&["a", "b", "c", "d"]), auto);
        let new_id =
            split_tab_pane(&mut tab, "d", Horizontal, SplitPlace::Second, None).expect("split");
        let mut panes = bound(&["a", "b", "c", "d"]);
        panes.push((new_id, None));
        assert_eq!(auto_grid_cols(4), 2);
        assert_eq!(auto_grid_cols(5), 3);
        assert_eq!(grid_of(&tab), &build_grid(&panes, 3, None).expect("grid"));
        assert_eq!(tab.picked_layout, Some(auto), "the pick stays automatic");
    }

    /// An automatic pick remembers the pane area's aspect, so a later pane
    /// count re-picks from it rather than from `ceil(sqrt(N))`.
    #[test]
    fn split_pane_on_an_untouched_auto_grid_re_picks_from_its_stored_aspect() {
        let tall_auto = RearrangeLayout::Grid {
            cols: 0,
            aspect: Some(0.5),
        };
        let mut tab = picked_tab("T", &bound(&["a", "b", "c"]), tall_auto);
        assert_eq!(
            grid_of(&tab),
            &build_grid(&bound(&["a", "b", "c"]), 1, None).expect("grid"),
            "three panes in an area half as wide as tall hold one column"
        );
        let new_id =
            split_tab_pane(&mut tab, "c", Horizontal, SplitPlace::Second, None).expect("split");
        let mut panes = bound(&["a", "b", "c"]);
        panes.push((new_id, None));
        assert_eq!(
            best_fit_grid_cols(4, 0.5),
            1,
            "four panes at 1:2 still fit one column"
        );
        assert_eq!(auto_grid_cols(4), 2, "ceil(sqrt(4)) would pick two");
        assert_eq!(
            grid_of(&tab),
            &build_grid(&panes, 0, Some(0.5)).expect("grid"),
            "the rebuild re-picks from the stored aspect"
        );
        assert_eq!(
            tab.picked_layout,
            Some(tall_auto),
            "the pick stays automatic"
        );
    }

    #[test]
    fn split_pane_on_an_untouched_fixed_grid_keeps_its_columns() {
        let two_cols = RearrangeLayout::Grid {
            cols: 2,
            aspect: None,
        };
        let mut tab = picked_tab("T", &bound(&["a", "b", "c"]), two_cols);
        let new_id =
            split_tab_pane(&mut tab, "c", Horizontal, SplitPlace::Second, None).expect("split");
        let mut panes = bound(&["a", "b", "c"]);
        panes.push((new_id, None));
        assert_eq!(
            grid_of(&tab),
            &build_grid(&panes, 2, None).expect("grid"),
            "a fixed column count stays fixed"
        );
        assert_eq!(tab.picked_layout, Some(two_cols));
    }

    /// A resize re-sends an automatic tab's grid: that is a re-fit, not a
    /// tidy-up, so it keeps the tab's empty panes and only re-picks the
    /// columns from the new pane area's shape.
    #[test]
    fn auto_rearrange_keeps_the_unbound_pane_and_re_picks_from_the_new_aspect() {
        let tall_auto = RearrangeLayout::Grid {
            cols: 0,
            aspect: Some(0.5),
        };
        let mut tab = picked_tab("T", &bound(&["a", "b", "c"]), tall_auto);
        let empty_id =
            split_tab_pane(&mut tab, "c", Horizontal, SplitPlace::Second, None).expect("split");
        let mut panes = bound(&["a", "b", "c"]);
        panes.push((empty_id.clone(), None));
        assert_eq!(
            collect_panes(grid_of(&tab)).len(),
            4,
            "the split left a pane"
        );

        let wide_auto = RearrangeLayout::Grid {
            cols: 0,
            aspect: Some(1.6),
        };
        pick_layout(&mut tab, wide_auto).expect("re-fit");
        assert_eq!(
            collect_panes(grid_of(&tab)),
            panes,
            "an automatic re-fit keeps the empty pane, in walk order"
        );
        assert_eq!(
            grid_of(&tab),
            &build_grid(&panes, 0, Some(1.6)).expect("grid"),
            "the columns are re-picked from the new aspect"
        );
        assert_eq!(
            tab.picked_layout,
            Some(wide_auto),
            "the new aspect is stored and the tab stays untouched"
        );

        // Untouched still means a later split re-picks from that aspect.
        let new_id = split_tab_pane(
            &mut tab,
            &empty_id,
            Horizontal,
            SplitPlace::Second,
            Some("s-d".to_string()),
        )
        .expect("split");
        panes.push((new_id, Some("s-d".to_string())));
        assert_eq!(
            grid_of(&tab),
            &build_grid(&panes, 0, Some(1.6)).expect("grid"),
            "the following split re-picks from the stored aspect"
        );
        assert_eq!(tab.picked_layout, Some(wide_auto));
    }

    /// Only an automatic re-fit keeps the empty panes: an explicit grid is
    /// still the user tidying the tab up.
    #[test]
    fn auto_tab_given_a_fixed_grid_still_drops_the_unbound_pane() {
        let mut tab = picked_tab(
            "T",
            &bound(&["a", "b", "c"]),
            RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(0.5),
            },
        );
        split_tab_pane(&mut tab, "c", Horizontal, SplitPlace::Second, None).expect("split");
        assert_eq!(collect_panes(grid_of(&tab)).len(), 4);

        let two_cols = RearrangeLayout::Grid {
            cols: 2,
            aspect: None,
        };
        pick_layout(&mut tab, two_cols).expect("pick");
        assert_eq!(
            collect_panes(grid_of(&tab)),
            bound(&["a", "b", "c"]),
            "a fixed pick drops the empty pane as before"
        );
        assert_eq!(tab.picked_layout, Some(two_cols));
    }

    #[test]
    fn close_pane_on_an_untouched_vertical_tab_rebalances() {
        let mut tab = picked_tab(
            "T",
            &bound(&["a", "b", "c", "d"]),
            RearrangeLayout::Vertical,
        );
        assert!(!close_tab_pane(&mut tab, "a").expect("close"));
        assert_eq!(
            grid_of(&tab),
            &build_balanced(&bound(&["b", "c", "d"]), Vertical).expect("grid")
        );
        assert_eq!(tab.picked_layout, Some(RearrangeLayout::Vertical));
    }

    #[test]
    fn close_session_panes_rebuilds_untouched_tabs_only() {
        let mut panes = bound(&["b", "c", "d"]);
        panes.insert(0, ("a".to_string(), Some("doomed".to_string())));
        let grid = build_balanced(&panes, Horizontal).expect("grid");
        let mut tabs = vec![
            picked_tab("picked", &panes, RearrangeLayout::Horizontal),
            grid_tab("touched", grid),
        ];

        let result = close_session_panes(&mut tabs, "doomed");

        assert_eq!(result.updated.len(), 2);
        assert_eq!(
            grid_of(&tabs[0]),
            &build_balanced(&bound(&["b", "c", "d"]), Horizontal).expect("grid")
        );
        assert_eq!(tabs[0].picked_layout, Some(RearrangeLayout::Horizontal));
        assert_eq!(
            grid_of(&tabs[1]),
            &split(
                Horizontal,
                0.5,
                pane("b", Some("s-b")),
                split(
                    Horizontal,
                    0.5,
                    pane("c", Some("s-c")),
                    pane("d", Some("s-d"))
                ),
            ),
            "the touched tab's sibling fills the space as before"
        );
        assert_eq!(
            result.updated[0], tabs[0],
            "the broadcast carries the rebuild"
        );
    }

    #[test]
    fn set_pane_ratio_clears_the_untouched_flag() {
        let mut tab = picked_tab("T", &bound(&["a", "b"]), RearrangeLayout::Horizontal);
        set_tab_pane_ratio(&mut tab, &[], 0.7).expect("set ratio");
        assert_eq!(tab.picked_layout, None);
        let GridNode::Split { ratio, .. } = grid_of(&tab) else {
            panic!("expected Split");
        };
        assert!((ratio - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn split_pane_on_a_touched_tab_keeps_todays_behaviour() {
        let mut tab = grid_tab(
            "T",
            build_balanced(&bound(&["a", "b"]), Horizontal).expect("grid"),
        );
        let new_id =
            split_tab_pane(&mut tab, "b", Vertical, SplitPlace::Second, None).expect("split");
        assert_eq!(
            grid_of(&tab),
            &split(
                Horizontal,
                0.5,
                pane("a", Some("s-a")),
                split(Vertical, 0.5, pane("b", Some("s-b")), pane(&new_id, None)),
            )
        );
        assert_eq!(tab.picked_layout, None);
    }

    #[test]
    fn reapply_keeps_pane_ids_order_and_empty_panes() {
        let grid = split(
            Vertical,
            0.3,
            pane("a", Some("s-a")),
            split(Horizontal, 0.8, pane("e", None), pane("b", Some("s-b"))),
        );
        let mut tab = TabEntry {
            picked_layout: Some(RearrangeLayout::Horizontal),
            ..grid_tab("T", grid)
        };
        assert!(reapply_if_untouched(&mut tab));
        let expected = vec![
            ("a".to_string(), Some("s-a".to_string())),
            ("e".to_string(), None),
            ("b".to_string(), Some("s-b".to_string())),
        ];
        assert_eq!(collect_panes(grid_of(&tab)), expected);
        assert_eq!(
            grid_of(&tab),
            &build_balanced(&expected, Horizontal).expect("grid")
        );
        assert_eq!(tab.picked_layout, Some(RearrangeLayout::Horizontal));

        let mut touched = grid_tab("U", pane("x", None));
        assert!(
            !reapply_if_untouched(&mut touched),
            "an unflagged tab is left alone"
        );
    }

    #[test]
    fn reapply_with_one_pane_left_is_a_single_pane() {
        let three_cols = RearrangeLayout::Grid {
            cols: 3,
            aspect: None,
        };
        let mut tab = picked_tab("T", &bound(&["a", "b"]), three_cols);
        assert!(!close_tab_pane(&mut tab, "b").expect("close"));
        assert_eq!(grid_of(&tab), &pane("a", Some("s-a")));
        assert_eq!(tab.picked_layout, Some(three_cols));
    }

    #[test]
    fn extract_to_new_tab_with_a_layout_marks_the_new_tab_and_rebuilds_an_untouched_source() {
        let mut tabs = vec![picked_tab(
            "src",
            &bound(&["a", "b", "c", "d", "e"]),
            RearrangeLayout::Horizontal,
        )];
        let outcome = extract_to_new_tab(
            &mut tabs,
            "src",
            &["a".to_string()],
            None,
            Some(RearrangeLayout::Vertical),
        )
        .expect("extract");
        assert!(outcome.source.is_some());
        let new_tab = outcome.new_tab;
        assert_eq!(new_tab.picked_layout, Some(RearrangeLayout::Vertical));
        assert_eq!(
            grid_of(&tabs[0]),
            &build_balanced(&bound(&["b", "c", "d", "e"]), Horizontal).expect("grid")
        );

        let unpicked = extract_to_new_tab(&mut tabs, "src", &["b".to_string()], None, None)
            .expect("extract without a layout")
            .new_tab;
        assert_eq!(
            unpicked.picked_layout, None,
            "no layout chosen, nothing picked"
        );
    }

    #[test]
    fn extract_to_new_tab_with_an_unknown_layout_remembers_the_built_layout() {
        let mut tabs = vec![picked_tab(
            "src",
            &bound(&["a", "b"]),
            RearrangeLayout::Horizontal,
        )];
        let outcome = extract_to_new_tab(
            &mut tabs,
            "src",
            &["a".to_string()],
            None,
            Some(RearrangeLayout::Unknown),
        )
        .expect("extract");
        assert_eq!(
            outcome.new_tab.picked_layout,
            Some(RearrangeLayout::Balanced)
        );
        assert_eq!(grid_of(&outcome.new_tab), &pane("a", Some("s-a")));
    }

    fn pane_move<'a>(
        src: (&'a str, &'a str),
        dst: (&'a str, &'a str),
        edge: PaneDropEdge,
    ) -> PaneMove<'a> {
        PaneMove {
            src_tab_id: src.0,
            src_pane_id: src.1,
            dst_tab_id: dst.0,
            dst_pane_id: dst.1,
            edge,
        }
    }

    #[test]
    fn move_pane_across_tabs_rebuilds_both_untouched_tabs() {
        let mut tabs = vec![
            picked_tab(
                "S",
                &bound(&["a", "b", "c", "d"]),
                RearrangeLayout::Horizontal,
            ),
            picked_tab("D", &bound(&["x", "y"]), RearrangeLayout::Vertical),
        ];
        let moved = move_pane(
            &mut tabs,
            &pane_move(("S", "a"), ("D", "y"), PaneDropEdge::Right),
        )
        .expect("move");
        assert_eq!(
            grid_of(&tabs[0]),
            &build_balanced(&bound(&["b", "c", "d"]), Horizontal).expect("grid")
        );
        assert_eq!(
            grid_of(&tabs[1]),
            &build_balanced(&bound(&["x", "y", "a"]), Vertical).expect("grid")
        );
        assert_eq!(tabs[1].picked_layout, Some(RearrangeLayout::Vertical));
        assert_eq!(
            moved,
            vec![
                MovedTab::Updated(tabs[0].clone()),
                MovedTab::Updated(tabs[1].clone())
            ]
        );
    }

    #[test]
    fn same_tab_move_clears_the_flag() {
        let mut tabs = vec![
            picked_tab("M", &bound(&["a", "b", "c"]), RearrangeLayout::Horizontal),
            picked_tab("W", &bound(&["x", "y"]), RearrangeLayout::Horizontal),
        ];
        move_pane(
            &mut tabs,
            &pane_move(("M", "a"), ("M", "c"), PaneDropEdge::Bottom),
        )
        .expect("move within the tab");
        move_pane(
            &mut tabs,
            &pane_move(("W", "x"), ("W", "y"), PaneDropEdge::Replace),
        )
        .expect("swap within the tab");
        assert_eq!(tabs[0].picked_layout, None, "a move clears the flag");
        assert_eq!(tabs[1].picked_layout, None, "a swap clears the flag");
    }

    #[test]
    fn merge_tabs_leaves_the_new_tab_unflagged() {
        let mut tabs = vec![
            picked_tab("A", &bound(&["a"]), RearrangeLayout::Horizontal),
            picked_tab("B", &bound(&["b"]), RearrangeLayout::Vertical),
        ];
        let merged = merge_tabs(
            &mut tabs,
            &["A".to_string(), "B".to_string()],
            None,
            MergeLayout::TileHorizontal,
        )
        .expect("merge");
        assert_eq!(merged.picked_layout, None);
    }

    #[test]
    fn clone_tabs_fresh_ids_keeps_the_flag() {
        let three_cols = RearrangeLayout::Grid {
            cols: 3,
            aspect: None,
        };
        let src = vec![
            picked_tab("P", &bound(&["a", "b"]), three_cols),
            grid_tab("U", pane("c", None)),
        ];
        let cloned = clone_tabs_fresh_ids(&src);
        assert_eq!(cloned[0].picked_layout, Some(three_cols));
        assert_eq!(cloned[1].picked_layout, None);
    }
}
