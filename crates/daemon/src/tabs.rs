//! Pure tree mutations on [`GridNode`] plus multi-tab helpers (merge / extract).
//!
//! All operations are pure on the in-memory tree; persistence and broadcast
//! happen in the dispatch layer (`server.rs`). The helpers panic-free and use
//! `anyhow::Result` to surface missing-id errors.

use anyhow::{Context as _, anyhow};
use chrono::Utc;
use protocol::{GridNode, MergeLayout, PaneDropEdge, SplitDirection, SplitPlace, TabEntry};
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

/// Build a fresh single-pane tab.
pub fn make_tab(name: Option<String>, initial_session_id: Option<String>) -> TabEntry {
    let id = new_id();
    let pane_id = new_id();
    let default_name = "Tab".to_string();
    TabEntry {
        id,
        name: name.unwrap_or(default_name),
        grid: GridNode::Pane {
            pane_id,
            session_id: initial_session_id,
        },
        created_at: Utc::now(),
    }
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
        collected.extend(collect_panes(&entry.grid));
    }
    let direction = match layout {
        MergeLayout::TileHorizontal => SplitDirection::Horizontal,
        MergeLayout::TileVertical => SplitDirection::Vertical,
    };
    let grid = build_balanced(&collected, direction)
        .ok_or_else(|| anyhow!("merge would produce empty tab"))?;
    tabs.retain(|t| !tab_ids.contains(&t.id));
    let entry = TabEntry {
        id: new_id(),
        name: name.unwrap_or_else(|| "Tab".to_string()),
        grid,
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
    let mut extracted: Vec<(String, Option<String>)> = Vec::with_capacity(pane_ids.len());
    let mut source_empty = false;
    for pid in pane_ids {
        let (node, empty) = extract_pane(&mut source.grid, pid)?;
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
    let entry = TabEntry {
        id: new_id(),
        name: name.unwrap_or_else(|| "Tab".to_string()),
        grid,
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
            grid,
            created_at: Utc::now(),
        }
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
        let panes = collect_panes(&tabs[0].grid);
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
        assert!(matches!(&source.grid, GridNode::Pane { pane_id, .. } if pane_id == "p2"));
        assert!(matches!(&new_tab.grid, GridNode::Pane { pane_id, .. } if pane_id == "p1"));
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
    fn reorder_tabs_rejects_mismatched_set() {
        let mut tabs = vec![
            make_tab_with("a", "A", pane("p1", None)),
            make_tab_with("b", "B", pane("p2", None)),
        ];
        assert!(reorder_tabs(&mut tabs, &["a".to_string()]).is_err());
        assert!(reorder_tabs(&mut tabs, &["a".to_string(), "z".to_string()]).is_err());
    }
}
