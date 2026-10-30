//! The tab strip and split panes as a plain model: the tabs the daemon keeps
//! for this client, the active tab, each tab's focused pane, where a session
//! is placed, and where a tab's panes and dividers sit.

use std::collections::{HashMap, HashSet, VecDeque};

use protocol::{
    ClientMessage, DaemonMessage, GridNode, PaneDropEdge, RearrangeLayout, SessionSnapshot,
    SessionStatus, SplitDirection, SplitPlace, TabEntry,
};

/// The divider ratios the daemon accepts; it clamps to the same range.
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;
/// What a ratio that is not a number becomes, as in the daemon.
const DEFAULT_RATIO: f32 = 0.5;
/// Placement measures a tab as a unit square, as the Tauri app does, so both
/// clients pick the same pane and direction.
const UNIT: Rect = Rect::new(0.0, 0.0, 1.0, 1.0);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn area(self) -> f32 {
        self.width * self.height
    }
}

/// A leaf of a split tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane<'a> {
    pub id: &'a str,
    pub session: Option<&'a str>,
}

/// A pane of any tab, with the tab it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "the names of the protocol fields they carry"
)]
pub struct PaneBinding {
    pub tab_id: String,
    pub pane_id: String,
    pub session_id: Option<String>,
}

/// The line between a split's two children.
#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    /// The first (0) or second (1) child taken at each split from the root
    /// down to this one, as the daemon's `tabs::set_pane_ratio` walks it.
    pub split_path: Vec<u8>,
    pub direction: SplitDirection,
    /// The whole split; a drag position maps to a ratio through it.
    pub split: Rect,
    /// The divider itself, centred on the boundary between the children.
    pub rect: Rect,
}

/// A pane to split and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitTarget {
    pub pane_id: String,
    pub direction: SplitDirection,
    pub place: SplitPlace,
}

/// Where a session goes inside a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTarget {
    /// Fill this empty pane.
    Replace {
        pane_id: String,
    },
    Split(SplitTarget),
}

/// Where a session the layout does not show yet goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Pane {
        tab_id: String,
        target: PaneTarget,
    },
    /// No tab can take it: a new tab holds it.
    NewTab,
}

/// What the panes' sessions need after a layout change.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionChanges {
    /// No pane shows these any more.
    pub detach: Vec<String>,
    /// Fewer panes show these, but some still do.
    pub resync: Vec<String>,
}

/// The two-click close of a tab worth keeping: one armed tab at a time.
#[derive(Debug, Default)]
pub struct CloseConfirm {
    armed: Option<String>,
}

/// How a click on a tab's pill was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillClick {
    /// No modifier: shows the tab.
    Plain,
    /// Ctrl: adds the tab to the selection or takes it out.
    Toggle,
    /// Shift: selects the tabs from the anchor to this one.
    Range,
}

/// The tabs picked for an action on several at once, and the anchor a
/// Shift+click ranges from. The active tab is not selected by being shown.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TabSelection {
    selected: Vec<String>,
    anchor: Option<String>,
    /// The tab shown when the anchor was set; once another is shown, the
    /// shown tab is the anchor.
    anchor_active: Option<String>,
}

/// The tab list, the active tab and each tab's focused pane.
#[derive(Debug, Default)]
pub struct TabsModel {
    tabs: Vec<TabEntry>,
    loaded: bool,
    active: Option<String>,
    /// The active tab saved by the last run, used once on the first list.
    restored_active: Option<String>,
    focused: HashMap<String, String>,
    /// The requests for new tabs this client sent, oldest first; each new
    /// tab to arrive consumes the oldest and becomes active. A request the
    /// daemon refuses is taken out, so it activates nothing.
    pending_create: VecDeque<String>,
    /// Tabs a restore named, by id, with the pane each focuses when it
    /// arrives. Unlike [`TabsModel::pending_create`], which any new tab's
    /// arrival consumes, only the tab with the named id consumes one.
    pending_restore: HashMap<String, Option<String>>,
    /// A pane the view should give the keyboard to.
    focus_request: Option<String>,
    pub close_confirm: CloseConfirm,
    pub selection: TabSelection,
    /// Tabs this client asked to close whose `TabRemoved` has not arrived:
    /// still in the list, but gone as far as a restore is concerned, since
    /// the daemon handles the removal before anything sent after it.
    closing: HashSet<String>,
    /// The request whose send marked a tab in [`TabsModel::closing`], by tab
    /// id: the `Error` refusing that request clears the mark. A mark set
    /// without a request has no entry, and no refusal clears it.
    closing_requests: HashMap<String, String>,
    /// The splits, moves and extracts this client sent that no reply has
    /// answered yet: one entry per tab each names, with the request's id
    /// when it carries one. A tab one names is not marked closing, since
    /// the request may keep it.
    layout_in_flight: Vec<(String, Option<String>)>,
    /// The reorders this client sent that no echo has settled, oldest
    /// first: the request's id, the strip's order before it applied and the
    /// order it sent. A refusal hands the order it found to the entry after
    /// it, or puts it back when it is the last one pending.
    pending_reorders: VecDeque<(String, Vec<String>, Vec<String>)>,
}

/// `ratio` inside the daemon's range; a ratio that is not a number becomes
/// an even split, as in the daemon's `tabs::clamp_ratio`.
pub fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_nan() {
        DEFAULT_RATIO
    } else {
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    }
}

/// The panes of `grid`, first child before second (left to right, top to
/// bottom).
pub fn collect_panes(grid: &GridNode) -> Vec<Pane<'_>> {
    let mut out = Vec::new();
    walk_panes(grid, &mut out);
    out
}

fn walk_panes<'a>(node: &'a GridNode, out: &mut Vec<Pane<'a>>) {
    match node {
        GridNode::Pane {
            pane_id,
            session_id,
            ..
        } => out.push(Pane {
            id: pane_id,
            session: session_id.as_deref(),
        }),
        GridNode::Split { first, second, .. } => {
            walk_panes(first, out);
            walk_panes(second, out);
        }
    }
}

/// How far the pane area's aspect must move from the one an automatic grid
/// was picked at before the grid re-picks its columns: 0.01, so a rounding
/// wobble is not a resize.
pub const AUTO_GRID_ASPECT_EPSILON: f32 = 0.01;

/// Whether `tab`'s pick is an automatic grid: it names no column count, so
/// its columns follow the pane area's shape.
pub fn is_auto_grid(tab: &TabEntry) -> bool {
    matches!(
        tab.picked_layout,
        Some(RearrangeLayout::Grid { cols: 0, .. })
    )
}

/// The auto-grid re-picks a resize asks for: one `Grid { cols: 0, aspect }`
/// per automatic tab whose stored aspect is absent or differs from `aspect`
/// by [`AUTO_GRID_ASPECT_EPSILON`] or more, in tab order. A tab picked with a
/// fixed count, another layout, or never picked is left alone.
pub fn auto_grid_resends(tabs: &[TabEntry], aspect: f32) -> Vec<(String, RearrangeLayout)> {
    tabs.iter()
        .filter_map(|tab| {
            let Some(RearrangeLayout::Grid {
                cols: 0,
                aspect: stored,
            }) = tab.picked_layout
            else {
                return None;
            };
            if stored.is_some_and(|stored| (stored - aspect).abs() < AUTO_GRID_ASPECT_EPSILON) {
                return None;
            }
            Some((
                tab.id.clone(),
                RearrangeLayout::Grid {
                    cols: 0,
                    aspect: Some(aspect),
                },
            ))
        })
        .collect()
}

/// The rectangles of a split's two children inside `bounds`, laid out as the
/// grid view draws them: the first takes `ratio` of the split, then comes a
/// divider `thickness` wide, and the second takes what is left.
fn split_bounds(
    direction: SplitDirection,
    ratio: f32,
    bounds: Rect,
    thickness: f32,
) -> (Rect, Rect) {
    match direction {
        SplitDirection::Horizontal => {
            let width = bounds.width * ratio;
            (
                Rect { width, ..bounds },
                Rect {
                    x: bounds.x + width + thickness,
                    width: (bounds.width - width - thickness).max(0.0),
                    ..bounds
                },
            )
        }
        SplitDirection::Vertical => {
            let height = bounds.height * ratio;
            (
                Rect { height, ..bounds },
                Rect {
                    y: bounds.y + height + thickness,
                    height: (bounds.height - height - thickness).max(0.0),
                    ..bounds
                },
            )
        }
    }
}

/// Every pane's rectangle when `grid` fills `bounds` with dividers
/// `thickness` wide, in [`collect_panes`] order.
pub fn pane_rects(grid: &GridNode, bounds: Rect, thickness: f32) -> Vec<(String, Rect)> {
    let mut out = Vec::new();
    walk_rects(grid, bounds, thickness, &mut out);
    out
}

fn walk_rects(node: &GridNode, bounds: Rect, thickness: f32, out: &mut Vec<(String, Rect)>) {
    match node {
        GridNode::Pane { pane_id, .. } => out.push((pane_id.clone(), bounds)),
        GridNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_bounds(*direction, *ratio, bounds, thickness);
            walk_rects(first, a, thickness, out);
            walk_rects(second, b, thickness, out);
        }
    }
}

/// The sides of a pane that lie on the boundary of the grid it sits in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent sides, not a state machine"
)]
pub struct OuterSides {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// Every pane's sides that touch the grid's boundary when `grid` fills
/// `bounds` with dividers `thickness` wide, in [`pane_rects`] order. A side
/// touches when it lies within half a pixel of the bound, since the pane
/// rectangles are sums and differences of `f32` measures.
pub fn outer_edges(grid: &GridNode, bounds: Rect, thickness: f32) -> Vec<(String, OuterSides)> {
    const TOUCH_EPSILON: f32 = 0.5;
    let right = bounds.x + bounds.width;
    let bottom = bounds.y + bounds.height;
    pane_rects(grid, bounds, thickness)
        .into_iter()
        .map(|(pane_id, rect)| {
            let sides = OuterSides {
                left: (rect.x - bounds.x).abs() <= TOUCH_EPSILON,
                right: (rect.x + rect.width - right).abs() <= TOUCH_EPSILON,
                top: (rect.y - bounds.y).abs() <= TOUCH_EPSILON,
                bottom: (rect.y + rect.height - bottom).abs() <= TOUCH_EPSILON,
            };
            (pane_id, sides)
        })
        .collect()
}

/// Every split's divider when `grid` fills `bounds`, parents before their
/// children.
pub fn dividers(grid: &GridNode, bounds: Rect, thickness: f32) -> Vec<Divider> {
    let mut out = Vec::new();
    walk_dividers(grid, bounds, thickness, &mut Vec::new(), &mut out);
    out
}

fn walk_dividers(
    node: &GridNode,
    bounds: Rect,
    thickness: f32,
    path: &mut Vec<u8>,
    out: &mut Vec<Divider>,
) {
    let GridNode::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    else {
        return;
    };
    let (a, b) = split_bounds(*direction, *ratio, bounds, thickness);
    let rect = match direction {
        SplitDirection::Horizontal => Rect {
            x: a.x + a.width,
            width: thickness,
            ..bounds
        },
        SplitDirection::Vertical => Rect {
            y: a.y + a.height,
            height: thickness,
            ..bounds
        },
    };
    out.push(Divider {
        split_path: path.clone(),
        direction: *direction,
        split: bounds,
        rect,
    });
    for (step, child, child_bounds) in [(0, first, a), (1, second, b)] {
        path.push(step);
        walk_dividers(child, child_bounds, thickness, path, out);
        path.pop();
    }
}

/// The ratio `divider`'s split takes when the divider's middle is dragged to
/// `(x, y)`, in the daemon's range.
pub fn ratio_at(divider: &Divider, x: f32, y: f32) -> f32 {
    let (offset, extent) = match divider.direction {
        SplitDirection::Horizontal => (
            x - divider.split.x - divider.rect.width / 2.0,
            divider.split.width,
        ),
        SplitDirection::Vertical => (
            y - divider.split.y - divider.rect.height / 2.0,
            divider.split.height,
        ),
    };
    if extent > 0.0 {
        clamp_ratio(offset / extent)
    } else {
        DEFAULT_RATIO
    }
}

/// Sets the ratio of the split `split_path` leads to, as the daemon's
/// `tabs::set_pane_ratio` does; false when the path does not end at a split.
pub fn set_ratio(grid: &mut GridNode, split_path: &[u8], ratio: f32) -> bool {
    let mut node = grid;
    for step in split_path {
        let GridNode::Split { first, second, .. } = node else {
            return false;
        };
        node = match step {
            0 => first.as_mut(),
            1 => second.as_mut(),
            _ => return false,
        };
    }
    let GridNode::Split { ratio: r, .. } = node else {
        return false;
    };
    *r = clamp_ratio(ratio);
    true
}

/// The direction that halves `rect` along its longer side (a square splits
/// left/right).
pub fn balance_split_direction(rect: Rect) -> SplitDirection {
    if rect.width >= rect.height {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    }
}

/// The first session, left to right, in the subtree `pane_id` sits beside
/// at its parent split: the pane it was split off from. None when
/// `pane_id` is the root pane or not in `grid`, or when that subtree shows
/// no session. Ported from `findSplitSiblingSession` in the Tauri app's
/// `utils/grid.ts`.
pub fn split_sibling_session<'a>(grid: &'a GridNode, pane_id: &str) -> Option<&'a str> {
    let GridNode::Split { first, second, .. } = grid else {
        return None;
    };
    let is_it =
        |node: &GridNode| matches!(node, GridNode::Pane { pane_id: id, .. } if id == pane_id);
    if is_it(first) {
        return first_session(second);
    }
    if is_it(second) {
        return first_session(first);
    }
    split_sibling_session(first, pane_id).or_else(|| split_sibling_session(second, pane_id))
}

fn first_session(node: &GridNode) -> Option<&str> {
    collect_panes(node)
        .into_iter()
        .find_map(|pane| pane.session)
}

/// The largest pane (the first of equals), split along its longer side with
/// the new pane second.
pub fn pick_balanced_split_target(grid: &GridNode) -> Option<SplitTarget> {
    let mut best: Option<(String, Rect)> = None;
    for (pane_id, rect) in pane_rects(grid, UNIT, 0.0) {
        if best.as_ref().is_none_or(|(_, b)| rect.area() > b.area()) {
            best = Some((pane_id, rect));
        }
    }
    best.map(|(pane_id, rect)| SplitTarget {
        pane_id,
        direction: balance_split_direction(rect),
        place: SplitPlace::Second,
    })
}

/// Where a pane moved into `grid` lands: the first empty pane, which it
/// replaces, else the pane [`pick_balanced_split_target`] picks, split on
/// its right or its bottom. Ported from `pickBalancedDropTarget` in the
/// Tauri app's `utils/grid.ts`.
pub fn pick_balanced_drop_target(grid: &GridNode) -> Option<(String, PaneDropEdge)> {
    if let Some(empty) = collect_panes(grid).iter().find(|p| p.session.is_none()) {
        return Some((empty.id.to_owned(), PaneDropEdge::Replace));
    }
    let target = pick_balanced_split_target(grid)?;
    let edge = match target.direction {
        SplitDirection::Horizontal => PaneDropEdge::Right,
        SplitDirection::Vertical => PaneDropEdge::Bottom,
    };
    Some((target.pane_id, edge))
}

/// The first tab, and its pane, that shows `session_id`.
pub fn find_tab_containing_session(
    tabs: &[TabEntry],
    session_id: &str,
) -> Option<(String, String)> {
    tabs.iter().find_map(|tab| {
        let grid = tab.grid()?;
        collect_panes(grid)
            .into_iter()
            .find(|p| p.session == Some(session_id))
            .map(|p| (tab.id.clone(), p.id.to_owned()))
    })
}

/// Which tabs show a session, as its sidebar leaf's pill tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabPill {
    /// No pane of any tab shows it.
    Unbound,
    /// One pane shows it.
    One { tab_id: String, name: String },
    /// Several panes show it: the name of each one's tab, in tab and pane
    /// order.
    Many(Vec<String>),
}

impl TabPill {
    /// The pill's hover text: the tab it opens in, or how many panes show
    /// the session and in which tabs, each named once.
    #[must_use]
    pub fn hover(&self) -> String {
        match self {
            Self::Unbound => {
                "No tab references this session — click to open it in a new tab".to_owned()
            }
            Self::One { name, .. } => format!("Open in tab \"{name}\""),
            Self::Many(names) => {
                let mut tabs: Vec<&str> = Vec::new();
                for name in names {
                    if !tabs.contains(&name.as_str()) {
                        tabs.push(name);
                    }
                }
                format!("Open in {} panes: {}", names.len(), tabs.join(", "))
            }
        }
    }
}

/// Every session a pane shows, with its pill, counting every pane that
/// shows it; a session missing from the map is unbound.
pub fn tab_pills(tabs: &[TabEntry]) -> HashMap<String, TabPill> {
    let mut shown: HashMap<&str, Vec<&TabEntry>> = HashMap::new();
    for tab in tabs {
        for pane in tab.grid().map(collect_panes).unwrap_or_default() {
            if let Some(session) = pane.session {
                shown.entry(session).or_default().push(tab);
            }
        }
    }
    shown
        .into_iter()
        .map(|(session, tabs)| {
            let pill = match tabs.as_slice() {
                [tab] => TabPill::One {
                    tab_id: tab.id.clone(),
                    name: tab.name.clone(),
                },
                many => TabPill::Many(many.iter().map(|tab| tab.name.clone()).collect()),
            };
            (session.to_owned(), pill)
        })
        .collect()
}

/// The pane to focus in `tab`: the remembered one while it still exists,
/// else the first. None for a tab without panes.
pub fn resolve_tab_focus(tab: Option<&TabEntry>, remembered: Option<&str>) -> Option<String> {
    let panes = collect_panes(tab?.grid()?);
    remembered
        .filter(|id| panes.iter().any(|p| p.id == *id))
        .or_else(|| panes.first().map(|p| p.id))
        .map(str::to_owned)
}

/// What a session belongs to for placement: its workspace, else its first
/// repo.
pub fn session_parent_key(session: &SessionSnapshot) -> Option<String> {
    if let Some(workspace) = &session.workspace_id {
        return Some(format!("workspace:{workspace}"));
    }
    session
        .members
        .first()
        .map(|member| format!("repo:{}", member.repo_id))
}

/// Where `session` goes in `grid`: beside a pane of the same repo or
/// workspace (an empty neighbour, else a split of the last such pane), else
/// the first empty pane, else a split of the largest pane.
pub fn pane_target_for_session(
    grid: &GridNode,
    sessions: &[SessionSnapshot],
    session: &SessionSnapshot,
) -> Option<PaneTarget> {
    let panes = collect_panes(grid);
    if let Some(key) = session_parent_key(session)
        && let Some(target) = same_parent_target(grid, &panes, sessions, &key)
    {
        return Some(target);
    }
    if let Some(empty) = panes.iter().find(|p| p.session.is_none()) {
        return Some(PaneTarget::Replace {
            pane_id: empty.id.to_owned(),
        });
    }
    pick_balanced_split_target(grid).map(PaneTarget::Split)
}

fn same_parent_target(
    grid: &GridNode,
    panes: &[Pane<'_>],
    sessions: &[SessionSnapshot],
    key: &str,
) -> Option<PaneTarget> {
    let parent_of = |id: &str| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(session_parent_key)
    };
    let matching: Vec<usize> = panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.session.and_then(parent_of).as_deref() == Some(key))
        .map(|(index, _)| index)
        .collect();
    let empty_at = |index: usize| panes.get(index).filter(|p| p.session.is_none());
    for &index in &matching {
        let neighbour = empty_at(index + 1).or_else(|| index.checked_sub(1).and_then(empty_at));
        if let Some(pane) = neighbour {
            return Some(PaneTarget::Replace {
                pane_id: pane.id.to_owned(),
            });
        }
    }
    let last = panes.get(*matching.last()?)?;
    let direction = pane_rects(grid, UNIT, 0.0)
        .into_iter()
        .find(|(id, _)| id == last.id)
        .map_or(SplitDirection::Horizontal, |(_, rect)| {
            balance_split_direction(rect)
        });
    Some(PaneTarget::Split(SplitTarget {
        pane_id: last.id.to_owned(),
        direction,
        place: SplitPlace::Second,
    }))
}

/// How many panes show each session.
pub fn view_counts<'a>(sessions: impl IntoIterator<Item = &'a str>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for id in sessions {
        *counts.entry(id.to_owned()).or_insert(0) += 1;
    }
    counts
}

/// The sessions that lost every pane (detach them) or some panes (resync
/// the rest), sorted.
pub fn session_changes(
    before: &HashMap<String, usize>,
    after: &HashMap<String, usize>,
) -> SessionChanges {
    let mut ids: Vec<(&String, usize)> = before.iter().map(|(id, n)| (id, *n)).collect();
    ids.sort();
    let mut changes = SessionChanges::default();
    for (id, was) in ids {
        let now = after.get(id).copied().unwrap_or(0);
        if now == 0 && was > 0 {
            changes.detach.push(id.clone());
        } else if now < was {
            changes.resync.push(id.clone());
        }
    }
    changes
}

/// Whether `session` is doing something worth surfacing: working, idle with
/// its turn's background work still running, waiting for an answer, or
/// still starting.
fn is_busy(session: &SessionSnapshot) -> bool {
    match session.status {
        SessionStatus::Working | SessionStatus::AwaitingInput | SessionStatus::Spawning => true,
        SessionStatus::Idle => session.background,
        SessionStatus::Stopped | SessionStatus::Error => false,
    }
}

/// The busy and total live terminals of `tab`, as `(busy, total)`: every
/// pane holding a session the daemon lists and has not marked inactive
/// counts, so a session shown in two panes counts twice. A session marked
/// "Don't count as busy" counts in total but never in busy. `None` for a tab
/// that is not a pane grid (a diff tab).
#[must_use]
pub fn tab_session_counts(tab: &TabEntry, sessions: &[SessionSnapshot]) -> Option<(usize, usize)> {
    let grid = tab.grid()?;
    let mut busy = 0;
    let mut total = 0;
    for pane in collect_panes(grid) {
        let Some(id) = pane.session else {
            continue;
        };
        let Some(session) = sessions.iter().find(|s| s.id == id) else {
            continue;
        };
        if session.is_inactive {
            continue;
        }
        total += 1;
        if is_busy(session) && !session.busy_excluded {
            busy += 1;
        }
    }
    Some((busy, total))
}

/// How many of `grid`'s panes hold a session.
#[must_use]
pub fn bound_pane_count(grid: &GridNode) -> usize {
    collect_panes(grid)
        .iter()
        .filter(|pane| pane.session.is_some())
        .count()
}

/// Whether closing `tab` loses something worth a second click: a bound
/// session, or a layout of two panes or more.
pub fn needs_confirm(tab: &TabEntry) -> bool {
    tab.grid().is_some_and(|grid| {
        let panes = collect_panes(grid);
        panes.len() >= 2 || panes.iter().any(|p| p.session.is_some())
    })
}

/// The terminal that sizes each session's PTY, by session: a pane open in a
/// pop-out window of its own (one of `popped_panes`), whichever tab it is
/// in, else the terminal of the session's own pop-out (one of
/// `popout_sessions`, named by [`crate::popouts::session_slot_id`]), else
/// the focused pane when it shows the session, else the first pane of the
/// active tab that does. Other hidden tabs drive nothing.
pub fn size_drivers(
    bindings: &[PaneBinding],
    active_tab: Option<&str>,
    focused_pane: Option<&str>,
    popped_panes: &[&str],
    popout_sessions: &[&str],
) -> HashMap<String, String> {
    let mut drivers = HashMap::new();
    let mut drive = |session_id: &str, view: String| {
        drivers.entry(session_id.to_owned()).or_insert(view);
    };
    let visible: Vec<&PaneBinding> = bindings
        .iter()
        .filter(|b| Some(b.tab_id.as_str()) == active_tab)
        .collect();
    let popped = bindings
        .iter()
        .filter(|b| popped_panes.contains(&b.pane_id.as_str()));
    for binding in popped {
        if let Some(session_id) = &binding.session_id {
            drive(session_id, binding.pane_id.clone());
        }
    }
    for session_id in popout_sessions {
        drive(session_id, crate::popouts::session_slot_id(session_id));
    }
    let focused = visible
        .iter()
        .copied()
        .filter(|b| Some(b.pane_id.as_str()) == focused_pane);
    for binding in focused.chain(visible.iter().copied()) {
        if let Some(session_id) = &binding.session_id {
            drive(session_id, binding.pane_id.clone());
        }
    }
    drivers
}

impl Placement {
    /// The request that puts `session_id` there. A new tab's `CreateTab`
    /// carries `request_id`, the id its arrival was armed with; a split
    /// carries a fresh id of its own, which the `Error` refusing it echoes;
    /// a pane replacement carries none.
    pub fn message(&self, session_id: &str, request_id: Option<String>) -> ClientMessage {
        let session_id = Some(session_id.to_owned());
        match self {
            Self::NewTab => ClientMessage::CreateTab {
                name: None,
                initial_session_id: session_id,
                host: None,
                request_id,
            },
            Self::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => ClientMessage::ReplacePaneSession {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
                session_id,
                host: None,
            },
            Self::Pane {
                tab_id,
                target: PaneTarget::Split(split),
            } => ClientMessage::SplitPane {
                tab_id: tab_id.clone(),
                pane_id: split.pane_id.clone(),
                direction: split.direction,
                place: split.place,
                new_session_id: session_id,
                host: None,
                request_id: Some(crate::new_request_id()),
            },
        }
    }
}

impl CloseConfirm {
    /// A close click on `tab`; returns whether to close it now. A tab worth
    /// keeping arms on the first click and closes on the second.
    pub fn click(&mut self, tab: &TabEntry) -> bool {
        if needs_confirm(tab) && self.armed.as_deref() != Some(tab.id.as_str()) {
            self.armed = Some(tab.id.clone());
            return false;
        }
        self.armed = None;
        true
    }

    /// Returns whether a tab was armed.
    pub fn disarm(&mut self) -> bool {
        self.armed.take().is_some()
    }

    pub fn armed(&self) -> Option<&str> {
        self.armed.as_deref()
    }
}

impl TabSelection {
    /// A `kind` click on `tab_id` in a strip ordered as `order`; returns
    /// whether the click shows the tab. A plain click clears the selection
    /// and anchors there; Ctrl toggles the tab and anchors there; Shift
    /// selects the anchor's range, or acts as a plain click with no anchor
    /// in the strip. `active` is the tab shown now: when it is not the one
    /// shown as the anchor was set (a shortcut, a spawn or a merge showed
    /// another since), Shift ranges from it instead.
    pub fn click(
        &mut self,
        order: &[&str],
        active: Option<&str>,
        tab_id: &str,
        kind: PillClick,
    ) -> bool {
        match kind {
            PillClick::Toggle => {
                if let Some(at) = self.selected.iter().position(|id| id == tab_id) {
                    self.selected.remove(at);
                } else {
                    self.selected.push(tab_id.to_owned());
                }
                self.set_anchor(tab_id, active);
                false
            }
            PillClick::Range => {
                if let Some(active) = active
                    && self.anchor_active.as_deref() != Some(active)
                {
                    self.set_anchor(active, Some(active));
                }
                let from = self
                    .anchor
                    .as_deref()
                    .and_then(|anchor| order.iter().position(|id| *id == anchor));
                let to = order.iter().position(|id| *id == tab_id);
                let (Some(from), Some(to)) = (from, to) else {
                    return self.click(order, active, tab_id, PillClick::Plain);
                };
                let (low, high) = (from.min(to), from.max(to));
                self.selected = order[low..=high]
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect();
                false
            }
            PillClick::Plain => {
                self.selected.clear();
                self.set_anchor(tab_id, Some(tab_id));
                true
            }
        }
    }

    fn set_anchor(&mut self, tab_id: &str, active: Option<&str>) {
        self.anchor = Some(tab_id.to_owned());
        self.anchor_active = active.map(str::to_owned);
    }

    /// Whether `tab_id` is selected.
    pub fn contains(&self, tab_id: &str) -> bool {
        self.selected.iter().any(|id| id == tab_id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    #[cfg(test)]
    pub fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }

    /// The selected tabs of `tabs`, in its order.
    pub fn in_order<'a>(&self, tabs: &'a [TabEntry]) -> Vec<&'a TabEntry> {
        tabs.iter().filter(|tab| self.contains(&tab.id)).collect()
    }

    /// Drops the selection; the anchor stays.
    pub fn clear(&mut self) {
        self.selected.clear();
    }

    /// Forgets every tab `live` no longer holds, the anchor included.
    fn prune(&mut self, live: &HashSet<&str>) {
        self.selected.retain(|id| live.contains(id.as_str()));
        if self
            .anchor
            .as_deref()
            .is_some_and(|anchor| !live.contains(anchor))
        {
            self.anchor = None;
        }
    }
}

impl TabsModel {
    pub fn new(restored_active: Option<String>) -> Self {
        Self {
            restored_active,
            ..Self::default()
        }
    }

    /// Forgets the tab list and everything waiting on it, as a switch to
    /// another daemon must: not loaded, no tabs, nothing pending, and
    /// `restored_active` as the tab the next list shows first.
    pub fn clear_for_switch(&mut self, restored_active: Option<String>) {
        *self = Self::new(restored_active);
    }

    /// Folds a tab message in; returns whether it was one.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Tabs { tabs } => self.replace_all(tabs),
            DaemonMessage::TabUpdated { tab } => self.upsert(tab),
            DaemonMessage::TabRemoved { tab_id } => self.remove(tab_id),
            DaemonMessage::TabsReordered { ordered_ids } => self.settle_reorder(ordered_ids),
            _ => return false,
        }
        true
    }

    /// A full list: the active tab stays while it exists; the first list
    /// restores the saved one; otherwise the first tab is active.
    fn replace_all(&mut self, tabs: &[TabEntry]) {
        tabs.clone_into(&mut self.tabs);
        self.loaded = true;
        let live: HashSet<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        self.pending_restore
            .retain(|id, _| live.contains(id.as_str()));
        self.focused
            .retain(|tab_id, _| live.contains(tab_id.as_str()));
        self.selection.prune(&live);
        self.closing.retain(|id| live.contains(id.as_str()));
        self.closing_requests
            .retain(|tab_id, _| live.contains(tab_id.as_str()));
        self.layout_in_flight
            .retain(|(tab_id, _)| live.contains(tab_id.as_str()));
        self.pending_reorders.clear();
        let active = self
            .active
            .take()
            .or_else(|| self.restored_active.take())
            .filter(|id| live.contains(id.as_str()))
            .or_else(|| self.tabs.first().map(|t| t.id.clone()));
        self.activate_id(active);
    }

    /// A tab created or changed. A new tab this client asked for, the first
    /// tab of all, or one a pending restore named, becomes active. It
    /// answers the oldest layout request in flight for the tab.
    fn upsert(&mut self, tab: &TabEntry) {
        if let Some(index) = self
            .layout_in_flight
            .iter()
            .position(|(tab_id, _)| *tab_id == tab.id)
        {
            self.layout_in_flight.remove(index);
        }
        if let Some(slot) = self.tabs.iter_mut().find(|t| t.id == tab.id) {
            let old = std::mem::replace(slot, tab.clone());
            self.focus_after_update(&old, tab);
            self.settle_restore(tab);
            return;
        }
        self.tabs.push(tab.clone());
        let requested = self.pending_create.pop_front().is_some();
        if requested || self.active.is_none() {
            self.activate_id(Some(tab.id.clone()));
        }
        self.settle_restore(tab);
    }

    /// A tab a pending restore named arrived: it shows, and the pane the
    /// snapshot focused takes the keyboard.
    fn settle_restore(&mut self, tab: &TabEntry) {
        let Some(focus) = self.pending_restore.remove(&tab.id) else {
            return;
        };
        if let Some(pane) = focus {
            self.set_focused(&tab.id, &pane);
        }
        self.activate_id(Some(tab.id.clone()));
    }

    /// A pane the update added takes its tab's focus. When the update takes
    /// away the pane the active tab focused, focus moves to what the tab
    /// resolves to now.
    fn focus_after_update(&mut self, old: &TabEntry, new: &TabEntry) {
        let is_active = self.active.as_deref() == Some(new.id.as_str());
        let old_panes = old.grid().map(collect_panes).unwrap_or_default();
        let new_panes = new.grid().map(collect_panes).unwrap_or_default();
        if let Some(fresh) = new_panes
            .iter()
            .find(|p| !old_panes.iter().any(|o| o.id == p.id))
        {
            self.focused.insert(new.id.clone(), fresh.id.to_owned());
            if is_active {
                self.focus_request = Some(fresh.id.to_owned());
            }
            return;
        }
        let remembered = self.focused.get(&new.id).map(String::as_str);
        let before = resolve_tab_focus(Some(old), remembered);
        let after = resolve_tab_focus(Some(new), remembered);
        if is_active && after != before {
            self.focus_request = after;
        }
    }

    /// A closed tab; when it was active, the first tab takes over. It
    /// answers every layout request in flight for the tab.
    fn remove(&mut self, tab_id: &str) {
        self.tabs.retain(|t| t.id != tab_id);
        self.focused.remove(tab_id);
        self.closing.remove(tab_id);
        self.closing_requests.remove(tab_id);
        self.layout_in_flight.retain(|(id, _)| id != tab_id);
        self.prune_selection();
        if self.close_confirm.armed() == Some(tab_id) {
            self.close_confirm.disarm();
        }
        if self.active.as_deref() == Some(tab_id) {
            let first = self.tabs.first().map(|t| t.id.clone());
            self.activate_id(first);
        }
    }

    /// The daemon's order; a tab it leaves out is dropped, as in the Tauri
    /// app.
    fn reorder(&mut self, ordered_ids: &[String]) {
        let mut by_id: HashMap<String, TabEntry> =
            self.tabs.drain(..).map(|t| (t.id.clone(), t)).collect();
        self.tabs = ordered_ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .collect();
        self.prune_selection();
    }

    /// `ordered` as the strip's order, applied before the daemon echoes it;
    /// the shown tab and the selection stay. A list that is not a
    /// permutation of the tabs changes nothing. Returns whether it applied.
    pub fn reorder_local(&mut self, ordered: &[String]) -> bool {
        let mut current: Vec<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        let mut wanted: Vec<&str> = ordered.iter().map(String::as_str).collect();
        current.sort_unstable();
        wanted.sort_unstable();
        if current != wanted {
            return false;
        }
        self.reorder(ordered);
        true
    }

    /// Remembers the strip's order as it stands and the order `request_id`'s
    /// reorder sends, before it applies: the refusal of that request puts
    /// the first back, unless a later reorder is still pending.
    pub fn note_reorder(&mut self, request_id: &str, sent_order: &[String]) {
        let order = self.tabs.iter().map(|tab| tab.id.clone()).collect();
        self.pending_reorders
            .push_back((request_id.to_owned(), order, sent_order.to_vec()));
    }

    /// The daemon refused the reorder `request_id`: it is no longer
    /// pending. A later reorder sent the order that follows the refused
    /// change, so its entry takes over the order the refused one found and
    /// the strip is left as it is; the last reorder pending puts its own
    /// order back.
    pub fn fail_reorder(&mut self, request_id: &str) {
        let Some(at) = self
            .pending_reorders
            .iter()
            .position(|(id, _, _)| id == request_id)
        else {
            return;
        };
        let Some((_, previous, _)) = self.pending_reorders.remove(at) else {
            return;
        };
        if let Some((_, next_previous, _)) = self.pending_reorders.get_mut(at) {
            *next_previous = previous;
        } else {
            self.restore_order(&previous);
        }
    }

    /// The daemon's answer to a reorder: it settles the oldest pending entry
    /// that sent that order, and every older entry goes with it, refused or
    /// superseded, since the daemon answers in order. The answer is the
    /// strip's order only once nothing newer is pending; otherwise the strip
    /// already shows the newer local one. An answer no entry sent — a
    /// restore's, or another window's reorder — is the strip's as it comes.
    fn settle_reorder(&mut self, ordered_ids: &[String]) {
        let Some(at) = self
            .pending_reorders
            .iter()
            .position(|(_, _, sent)| sent.as_slice() == ordered_ids)
        else {
            self.reorder(ordered_ids);
            return;
        };
        self.pending_reorders.drain(..=at);
        if self.pending_reorders.is_empty() {
            self.reorder(ordered_ids);
        }
    }

    /// `order` as the strip's order, keeping the tabs it no longer holds out
    /// and any tab it does not name after the ones it does; the shown tab
    /// and the selection stay.
    fn restore_order(&mut self, order: &[String]) {
        let mut slots: Vec<Option<TabEntry>> = std::mem::take(&mut self.tabs)
            .into_iter()
            .map(Some)
            .collect();
        let mut restored = Vec::with_capacity(slots.len());
        for id in order {
            let at = slots
                .iter()
                .position(|slot| slot.as_ref().is_some_and(|tab| &tab.id == id));
            if let Some(tab) = at.and_then(|at| slots[at].take()) {
                restored.push(tab);
            }
        }
        restored.extend(slots.into_iter().flatten());
        self.tabs = restored;
        self.prune_selection();
    }

    /// Forgets selected tabs the list no longer holds.
    fn prune_selection(&mut self) {
        let live: HashSet<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        self.selection.prune(&live);
    }

    fn activate_id(&mut self, tab_id: Option<String>) {
        self.active = tab_id;
        self.focus_request = self.active.as_deref().and_then(|id| self.focused_pane(id));
    }

    pub fn tabs(&self) -> &[TabEntry] {
        &self.tabs
    }

    /// Whether a full tab list has arrived on this or an earlier connection.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn tab(&self, id: &str) -> Option<&TabEntry> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn active_tab(&self) -> Option<&TabEntry> {
        self.tab(self.active.as_deref()?)
    }

    /// The pane `tab_id` focuses: the one last focused while it exists,
    /// else the first.
    pub fn focused_pane(&self, tab_id: &str) -> Option<String> {
        resolve_tab_focus(
            self.tab(tab_id),
            self.focused.get(tab_id).map(String::as_str),
        )
    }

    /// Records that `pane_id` took the keyboard in `tab_id`.
    pub fn set_focused(&mut self, tab_id: &str, pane_id: &str) {
        self.focused.insert(tab_id.to_owned(), pane_id.to_owned());
    }

    /// Shows `tab_id` and asks for its focused pane to take the keyboard.
    pub fn activate(&mut self, tab_id: &str) {
        if self.tab(tab_id).is_some() {
            self.activate_id(Some(tab_id.to_owned()));
        }
    }

    /// Shows `tab_id` with `pane_id` focused.
    pub fn focus_pane(&mut self, tab_id: &str, pane_id: &str) {
        if self.tab(tab_id).is_some() {
            self.set_focused(tab_id, pane_id);
            self.activate_id(Some(tab_id.to_owned()));
        }
    }

    /// The pane the view should give the keyboard to, once.
    pub fn take_focus_request(&mut self) -> Option<String> {
        self.focus_request.take()
    }

    /// The request `request_id` asks for a new tab: the next new tab to
    /// arrive is one this client asked for.
    pub fn arm_create(&mut self, request_id: &str) {
        self.pending_create.push_back(request_id.to_owned());
    }

    /// The daemon refused the request `request_id`: if it asked for a new
    /// tab whose arrival has not consumed it, no arrival activates for it.
    pub fn disarm_create(&mut self, request_id: &str) {
        self.pending_create.retain(|armed| armed != request_id);
    }

    /// Forgets every requested new tab: a fresh connection answers none of
    /// the old one's requests.
    pub fn clear_armed_creates(&mut self) {
        self.pending_create.clear();
    }

    /// The next arrival of `tab_id` shows it and gives `pane_id` the
    /// keyboard. Only that exact id consumes it, so a restore the daemon
    /// refuses leaves nothing armed for the next tab to arrive.
    pub fn activate_on_arrival(&mut self, tab_id: &str, focus_pane: Option<&str>) {
        self.pending_restore
            .insert(tab_id.to_owned(), focus_pane.map(str::to_owned));
    }

    /// Forgets every pending arrival and every closing tab: a fresh
    /// connection's list decides what shows and what still exists.
    pub fn clear_pending_restore(&mut self) {
        self.pending_restore.clear();
        self.closing.clear();
        self.closing_requests.clear();
    }

    /// The tabs whose close is on its way: still listed, but gone for a
    /// restore.
    pub fn closing(&self) -> &HashSet<String> {
        &self.closing
    }

    /// Records that this client asked to close `tab_id`. No refusal clears
    /// this mark, even one for a request that marked the tab before.
    pub fn mark_closing(&mut self, tab_id: &str) {
        self.closing.insert(tab_id.to_owned());
        self.closing_requests.remove(tab_id);
    }

    /// Marks `tab_id` closing when it holds a single pane: closing or moving
    /// that pane takes the whole tab with it. `request_id` is the id the
    /// request taking the pane carries, if any; its refusal clears the mark.
    pub fn mark_closing_if_last_pane(&mut self, tab_id: &str, request_id: Option<&str>) {
        let last_pane = self
            .tab(tab_id)
            .and_then(TabEntry::grid)
            .is_some_and(|grid| collect_panes(grid).len() == 1);
        if last_pane {
            self.mark_closing_for(tab_id, request_id);
        }
    }

    /// Marks `tab_id` closing when `pane_ids` holds every pane it has:
    /// moving them all out takes the whole tab with them. The refusal of
    /// `request_id` clears the mark.
    pub fn mark_closing_if_emptied(
        &mut self,
        tab_id: &str,
        pane_ids: &[String],
        request_id: Option<&str>,
    ) {
        let emptied = self
            .tab(tab_id)
            .and_then(TabEntry::grid)
            .is_some_and(|grid| {
                collect_panes(grid)
                    .iter()
                    .all(|pane| pane_ids.iter().any(|id| id == pane.id))
            });
        if emptied {
            self.mark_closing_for(tab_id, request_id);
        }
    }

    /// Marks `tab_id` closing for `request_id`, unless a layout request in
    /// flight may still keep the tab. A tab already marked keeps its mark
    /// as it was.
    fn mark_closing_for(&mut self, tab_id: &str, request_id: Option<&str>) {
        if self.layout_in_flight.iter().any(|(id, _)| id == tab_id) {
            return;
        }
        if self.closing.insert(tab_id.to_owned())
            && let Some(request_id) = request_id
        {
            self.closing_requests
                .insert(tab_id.to_owned(), request_id.to_owned());
        }
    }

    /// Records a split, move, swap, extract or snapshot restore this client
    /// is sending as in flight for each tab it names, until a reply about
    /// that tab answers it. A move or swap of a pane with itself gets no
    /// reply, so it is not recorded; every other message is ignored. A
    /// snapshot restore carries no id: its tab's update answers a success,
    /// and a refusal means the tab is gone, which its `TabRemoved` reports.
    pub fn note_layout_request(&mut self, msg: &ClientMessage) {
        let (tab_ids, request_id) = match msg {
            ClientMessage::SplitPane {
                tab_id, request_id, ..
            } => (vec![tab_id], request_id.as_ref()),
            ClientMessage::RestoreTabSnapshot { tab } => (vec![&tab.id], None),
            ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                request_id,
                ..
            } => {
                if src_pane_id == dst_pane_id {
                    return;
                }
                let mut tab_ids = vec![src_tab_id];
                if dst_tab_id != src_tab_id {
                    tab_ids.push(dst_tab_id);
                }
                (tab_ids, request_id.as_ref())
            }
            ClientMessage::SwapPanes {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                request_id,
            } => {
                if src_tab_id == dst_tab_id && src_pane_id == dst_pane_id {
                    return;
                }
                let mut tab_ids = vec![src_tab_id];
                if dst_tab_id != src_tab_id {
                    tab_ids.push(dst_tab_id);
                }
                (tab_ids, request_id.as_ref())
            }
            ClientMessage::ExtractToNewTab {
                source_tab_id,
                request_id,
                ..
            } => (vec![source_tab_id], request_id.as_ref()),
            _ => return,
        };
        for tab_id in tab_ids {
            self.layout_in_flight
                .push((tab_id.clone(), request_id.cloned()));
        }
    }

    /// The daemon refused the request `request_id`: it is no longer in
    /// flight, and a tab its send marked closing is not closing after all.
    pub fn fail_layout_request(&mut self, request_id: &str) {
        self.layout_in_flight
            .retain(|(_, id)| id.as_deref() != Some(request_id));
        let closing = &mut self.closing;
        self.closing_requests.retain(|tab_id, id| {
            let refused = id == request_id;
            if refused {
                closing.remove(tab_id);
            }
            !refused
        });
    }

    /// Forgets every layout request in flight: a fresh connection answers
    /// none of the old one's requests.
    pub fn clear_layout_requests(&mut self) {
        self.layout_in_flight.clear();
        self.pending_reorders.clear();
    }

    /// Where `session` goes: into the active tab by
    /// [`pane_target_for_session`], or a new tab when there is no active
    /// grid tab.
    pub fn place(&self, session: &SessionSnapshot, sessions: &[SessionSnapshot]) -> Placement {
        self.active.as_deref().map_or(Placement::NewTab, |tab_id| {
            self.place_in(tab_id, session, sessions)
        })
    }

    /// Where `session` goes in `tab_id` by [`pane_target_for_session`], or
    /// a new tab when `tab_id` is gone, closing, or holds no grid.
    pub fn place_in(
        &self,
        tab_id: &str,
        session: &SessionSnapshot,
        sessions: &[SessionSnapshot],
    ) -> Placement {
        if self.closing.contains(tab_id) {
            return Placement::NewTab;
        }
        let Some(tab) = self.tab(tab_id) else {
            return Placement::NewTab;
        };
        tab.grid()
            .and_then(|grid| pane_target_for_session(grid, sessions, session))
            .map_or(Placement::NewTab, |target| Placement::Pane {
                tab_id: tab.id.clone(),
                target,
            })
    }

    /// Every pane of every tab.
    pub fn bindings(&self) -> Vec<PaneBinding> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.grid().map(|grid| (tab, grid)))
            .flat_map(|(tab, grid)| {
                collect_panes(grid).into_iter().map(move |p| PaneBinding {
                    tab_id: tab.id.clone(),
                    pane_id: p.id.to_owned(),
                    session_id: p.session.map(str::to_owned),
                })
            })
            .collect()
    }

    /// How many panes, across every tab, show each session.
    pub fn view_counts(&self) -> HashMap<String, usize> {
        let bindings = self.bindings();
        view_counts(bindings.iter().filter_map(|b| b.session_id.as_deref()))
    }

    /// Sets a split's ratio locally, ahead of the daemon's echo.
    pub fn set_ratio(&mut self, tab_id: &str, split_path: &[u8], ratio: f32) -> bool {
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .and_then(TabEntry::grid_mut)
            .is_some_and(|grid| set_ratio(grid, split_path, ratio))
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
pub(crate) mod tests {
    use super::*;
    use crate::sidebar::UiState;
    use protocol::SessionMember;
    use serde_json::json;

    const H: SplitDirection = SplitDirection::Horizontal;
    const V: SplitDirection = SplitDirection::Vertical;

    pub(crate) fn pane(id: &str, session: Option<&str>) -> GridNode {
        GridNode::Pane {
            pane_id: id.to_owned(),
            session_id: session.map(str::to_owned),
            host: None,
        }
    }

    pub(crate) fn split(
        direction: SplitDirection,
        ratio: f32,
        first: GridNode,
        second: GridNode,
    ) -> GridNode {
        GridNode::Split {
            direction,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub(crate) fn tab(id: &str, grid: &GridNode) -> TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "grid", "grid": grid },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("tab fixture")
    }

    pub(crate) fn diff_tab(id: &str) -> TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "diff", "repo_id": "r", "path": "a.rs", "against": null },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("diff tab fixture")
    }

    #[test]
    fn clear_for_switch_empties_tabs_and_marks_unloaded() {
        let mut model = TabsModel::new(Some("t2".to_owned()));
        model.apply(&DaemonMessage::Tabs {
            tabs: vec![
                tab("t1", &pane("p1", Some("s1"))),
                tab("t2", &pane("p2", None)),
            ],
        });
        assert!(model.is_loaded());
        assert_eq!(model.active_id(), Some("t2"), "the restore hint applied");

        model.clear_for_switch(None);

        assert!(!model.is_loaded(), "the next host's list is awaited");
        assert!(model.tabs().is_empty(), "the old daemon's tabs are gone");
        assert_eq!(model.active_id(), None);
        assert_eq!(model.take_focus_request(), None);
        model.apply(&DaemonMessage::Tabs {
            tabs: vec![tab("h1", &pane("p1", None)), tab("t2", &pane("p2", None))],
        });
        assert_eq!(
            model.active_id(),
            Some("h1"),
            "nothing of the old list picks the new host's active tab"
        );

        model.clear_for_switch(Some("t2".to_owned()));
        model.apply(&DaemonMessage::Tabs {
            tabs: vec![tab("t1", &pane("p1", None)), tab("t2", &pane("p2", None))],
        });
        assert_eq!(
            model.active_id(),
            Some("t2"),
            "a switch back to this PC restores its saved tab"
        );
    }

    pub(crate) fn session(
        id: &str,
        repo: Option<&str>,
        workspace: Option<&str>,
    ) -> SessionSnapshot {
        let mut s: SessionSnapshot = serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture");
        s.members = repo
            .map(|r| SessionMember {
                repo_id: r.to_owned(),
                repo_name: r.to_owned(),
                branch: "main".to_owned(),
                worktree_path: String::new(),
            })
            .into_iter()
            .collect();
        s.workspace_id = workspace.map(str::to_owned);
        s
    }

    /// `ClientMessage` has no `PartialEq`; its wire form stands in.
    pub(crate) fn wire(msg: &ClientMessage) -> serde_json::Value {
        serde_json::to_value(msg).expect("serialize a client message")
    }

    fn tabs_msg(tabs: &[TabEntry]) -> DaemonMessage {
        DaemonMessage::Tabs {
            tabs: tabs.to_vec(),
        }
    }

    pub(crate) fn updated(tab: &TabEntry) -> DaemonMessage {
        DaemonMessage::TabUpdated { tab: tab.clone() }
    }

    pub(crate) fn model_with(tabs: &[TabEntry]) -> TabsModel {
        let mut model = TabsModel::new(None);
        assert!(model.apply(&tabs_msg(tabs)));
        model
    }

    /// `a` fills the left half; `b` (a quarter high) sits over `c` on the
    /// right.
    fn three() -> GridNode {
        split(
            H,
            0.5,
            pane("a", Some("s1")),
            split(V, 0.25, pane("b", None), pane("c", Some("s2"))),
        )
    }

    fn split_target(pane_id: &str, direction: SplitDirection) -> PaneTarget {
        PaneTarget::Split(SplitTarget {
            pane_id: pane_id.to_owned(),
            direction,
            place: SplitPlace::Second,
        })
    }

    fn replace(pane_id: &str) -> PaneTarget {
        PaneTarget::Replace {
            pane_id: pane_id.to_owned(),
        }
    }

    #[test]
    fn tabs_pane_rects_leave_room_for_dividers() {
        let rects = pane_rects(&three(), Rect::new(0.0, 0.0, 204.0, 104.0), 4.0);
        assert_eq!(
            rects,
            vec![
                ("a".to_owned(), Rect::new(0.0, 0.0, 102.0, 104.0)),
                ("b".to_owned(), Rect::new(106.0, 0.0, 98.0, 26.0)),
                ("c".to_owned(), Rect::new(106.0, 30.0, 98.0, 74.0)),
            ]
        );
    }

    #[test]
    fn outer_edges_of_a_two_by_two_grid() {
        let grid = split(
            V,
            0.5,
            split(H, 0.5, pane("a", None), pane("b", None)),
            split(H, 0.5, pane("c", None), pane("d", None)),
        );

        let found = outer_edges(&grid, Rect::new(0.0, 0.0, 204.0, 104.0), 4.0);

        let sides: Vec<(&str, OuterSides)> = found
            .iter()
            .map(|(id, sides)| (id.as_str(), *sides))
            .collect();
        assert_eq!(
            sides,
            [
                (
                    "a",
                    OuterSides {
                        left: true,
                        top: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "b",
                    OuterSides {
                        right: true,
                        top: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "c",
                    OuterSides {
                        left: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "d",
                    OuterSides {
                        right: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
            ],
            "each pane touches the two sides of the grid its column and row end at"
        );
    }

    #[test]
    fn outer_edges_of_a_lone_pane_is_all_four() {
        let found = outer_edges(&pane("a", None), Rect::new(10.0, 20.0, 100.0, 50.0), 4.0);
        assert_eq!(
            found,
            [(
                "a".to_owned(),
                OuterSides {
                    left: true,
                    right: true,
                    top: true,
                    bottom: true,
                }
            )],
            "a pane that fills the grid touches every side"
        );
    }

    #[test]
    fn outer_edges_with_uneven_ratios() {
        let found = outer_edges(&three(), Rect::new(0.0, 0.0, 204.0, 104.0), 4.0);
        assert_eq!(
            found,
            vec![
                (
                    "a".to_owned(),
                    OuterSides {
                        left: true,
                        top: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "b".to_owned(),
                    OuterSides {
                        right: true,
                        top: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "c".to_owned(),
                    OuterSides {
                        right: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
            ],
            "a nested pane touches only the sides its own edge of the grid ends at"
        );
    }

    #[test]
    fn outer_edges_accept_a_bound_a_fraction_off() {
        let bounds = Rect::new(3.3, 0.0, 1000.0, 100.0);
        let grid = split(H, 1.0 / 3.0, pane("a", None), pane("b", None));

        let rects = pane_rects(&grid, bounds, 4.0);
        let (_, far) = rects.get(1).expect("the second pane");
        let off = (far.x + far.width - (bounds.x + bounds.width)).abs();
        assert!(
            off > 0.0 && off < 0.5,
            "the split leaves the far side {off} off its bound, inside the tolerance"
        );

        assert_eq!(
            outer_edges(&grid, bounds, 4.0),
            vec![
                (
                    "a".to_owned(),
                    OuterSides {
                        left: true,
                        top: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
                (
                    "b".to_owned(),
                    OuterSides {
                        right: true,
                        top: true,
                        bottom: true,
                        ..OuterSides::default()
                    }
                ),
            ],
            "a side a fraction off its bound still touches the grid"
        );
    }

    /// The paths match `crates/daemon/src/tabs.rs::set_pane_ratio`: 0 takes
    /// the first child, 1 the second, from the root down to the split.
    #[test]
    fn tabs_divider_paths_follow_the_daemon_convention() {
        let grid = split(V, 0.5, three(), pane("d", None));
        let bounds = Rect::new(0.0, 0.0, 200.0, 200.0);
        let found = dividers(&grid, bounds, 4.0);
        let paths: Vec<Vec<u8>> = found.iter().map(|d| d.split_path.clone()).collect();
        assert_eq!(paths, vec![vec![], vec![0], vec![0, 1]]);
        let root = found.first().expect("root divider");
        assert_eq!(root.rect, Rect::new(0.0, 100.0, 200.0, 4.0));
        assert_eq!(root.split, bounds);
        let left_right = found.get(1).expect("divider of the top half");
        assert_eq!(left_right.rect, Rect::new(100.0, 0.0, 4.0, 100.0));
        assert_eq!(left_right.split, Rect::new(0.0, 0.0, 200.0, 100.0));
        assert_eq!(left_right.direction, H);
        let nested = found.get(2).expect("divider of the right column");
        assert_eq!(nested.split, Rect::new(104.0, 0.0, 96.0, 100.0));
        assert_eq!(nested.rect, Rect::new(104.0, 25.0, 96.0, 4.0));

        let mut edited = grid.clone();
        assert!(set_ratio(&mut edited, &[0, 1], 0.75));
        let rects = pane_rects(&edited, bounds, 4.0);
        assert_eq!(
            rects.get(1),
            Some(&("b".to_owned(), Rect::new(104.0, 0.0, 96.0, 75.0)))
        );
        assert!(!set_ratio(&mut edited, &[1], 0.3), "path ends at a pane");
        assert!(!set_ratio(&mut edited, &[0, 2], 0.3), "not a child index");
        assert!(set_ratio(&mut edited, &[], 2.0));
        assert_eq!(
            dividers(&edited, Rect::new(0.0, 0.0, 100.0, 100.0), 0.0)
                .first()
                .map(|d| d.rect.y),
            Some(95.0)
        );
    }

    #[test]
    fn tabs_drag_ratio_is_clamped_to_the_daemon_range() {
        let grid = split(H, 0.5, pane("a", None), pane("b", None));
        let found = dividers(&grid, Rect::new(100.0, 0.0, 200.0, 50.0), 4.0);
        let divider = found.first().expect("divider");
        assert!(
            (ratio_at(divider, 152.0, 10.0) - 0.25).abs() < 1e-6,
            "the pointer holds the divider by its middle"
        );
        assert!((ratio_at(divider, 0.0, 10.0) - MIN_RATIO).abs() < 1e-6);
        assert!((ratio_at(divider, 900.0, 10.0) - MAX_RATIO).abs() < 1e-6);
        assert!((clamp_ratio(f32::NAN) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tabs_nested_divider_drag_maps_through_its_own_split() {
        let grid = split(
            H,
            0.5,
            pane("a", None),
            split(V, 0.5, pane("b", None), pane("c", None)),
        );
        let found = dividers(&grid, Rect::new(0.0, 0.0, 204.0, 104.0), 4.0);
        let nested = found.get(1).expect("divider of the right column");
        assert_eq!(nested.split, Rect::new(106.0, 0.0, 98.0, 104.0));
        assert_eq!(nested.rect, Rect::new(106.0, 52.0, 98.0, 4.0));
        assert!((ratio_at(nested, 150.0, 28.0) - 0.25).abs() < 1e-6);
        assert!(
            (ratio_at(nested, 150.0, 54.0) - 0.5).abs() < 1e-6,
            "the drawn divider's middle is the current ratio"
        );
    }

    #[test]
    fn tabs_size_driver_is_the_focused_pane_else_the_first_visible() {
        let binding = |tab: &str, pane: &str, session: Option<&str>| PaneBinding {
            tab_id: tab.to_owned(),
            pane_id: pane.to_owned(),
            session_id: session.map(str::to_owned),
        };
        let bindings = [
            binding("t1", "a", Some("s1")),
            binding("t1", "b", Some("s2")),
            binding("t1", "c", Some("s1")),
            binding("t1", "e", None),
            binding("t2", "d", Some("s1")),
            binding("t2", "f", Some("s3")),
        ];
        let driver = |active: Option<&str>, focused: Option<&str>, session: &str| {
            size_drivers(&bindings, active, focused, &[], &[])
                .get(session)
                .cloned()
        };
        assert_eq!(driver(Some("t1"), Some("c"), "s1").as_deref(), Some("c"));
        assert_eq!(driver(Some("t1"), Some("c"), "s2").as_deref(), Some("b"));
        assert_eq!(
            driver(Some("t1"), Some("c"), "s3"),
            None,
            "a hidden tab drives nothing"
        );
        assert_eq!(driver(Some("t1"), Some("b"), "s1").as_deref(), Some("a"));
        assert_eq!(driver(Some("t1"), None, "s1").as_deref(), Some("a"));
        assert_eq!(
            driver(Some("t2"), Some("c"), "s1").as_deref(),
            Some("d"),
            "a focus outside the active tab does not count"
        );
        assert!(size_drivers(&bindings, None, None, &[], &[]).is_empty());
    }

    #[test]
    fn size_drivers_count_an_open_pane_popout() {
        let binding = |tab: &str, pane: &str, session: Option<&str>| PaneBinding {
            tab_id: tab.to_owned(),
            pane_id: pane.to_owned(),
            session_id: session.map(str::to_owned),
        };
        let bindings = [
            binding("t1", "a", Some("s1")),
            binding("t1", "b", Some("s2")),
            binding("t2", "d", Some("s1")),
            binding("t2", "e", None),
            binding("t2", "f", Some("s3")),
        ];
        let drivers = size_drivers(&bindings, Some("t1"), Some("a"), &["d", "e", "f"], &[]);

        assert_eq!(
            drivers.get("s1").map(String::as_str),
            Some("d"),
            "the popped pane drives its session over the active tab's focused pane"
        );
        assert_eq!(
            drivers.get("s3").map(String::as_str),
            Some("f"),
            "a popped pane in a hidden tab drives"
        );
        assert_eq!(
            drivers.get("s2").map(String::as_str),
            Some("b"),
            "a session no pop-out shows keeps its active-tab driver"
        );
        assert_eq!(drivers.len(), 3, "an empty popped pane drives nothing");
        assert_eq!(
            size_drivers(&bindings, Some("t1"), Some("a"), &["gone"], &[])
                .get("s1")
                .map(String::as_str),
            Some("a"),
            "a popped pane no binding names changes nothing"
        );
    }

    #[test]
    fn size_drivers_count_a_session_popout() {
        let binding = |tab: &str, pane: &str, session: Option<&str>| PaneBinding {
            tab_id: tab.to_owned(),
            pane_id: pane.to_owned(),
            session_id: session.map(str::to_owned),
        };
        let bindings = [
            binding("t1", "a", Some("s1")),
            binding("t1", "b", Some("s2")),
            binding("t2", "d", Some("s3")),
        ];
        let drivers = size_drivers(&bindings, Some("t1"), Some("a"), &[], &["s4", "s1"]);

        assert_eq!(
            drivers.get("s4").map(String::as_str),
            Some("popout:s4"),
            "a session no pane shows is driven by its pop-out's terminal"
        );
        assert_eq!(
            drivers.get("s1").map(String::as_str),
            Some("popout:s1"),
            "a session pop-out drives over the active tab's focused pane"
        );
        assert_eq!(drivers.get("s2").map(String::as_str), Some("b"));
        assert_eq!(drivers.get("s3"), None, "a hidden tab still drives nothing");
        assert_eq!(
            size_drivers(&bindings, None, None, &[], &["s4"])
                .get("s4")
                .map(String::as_str),
            Some("popout:s4"),
            "a session pop-out drives with no tab on screen"
        );
    }

    #[test]
    fn tabs_balance_direction_splits_the_longer_side() {
        assert_eq!(balance_split_direction(Rect::new(0.0, 0.0, 1.0, 1.0)), H);
        assert_eq!(balance_split_direction(Rect::new(0.0, 0.0, 3.0, 1.0)), H);
        assert_eq!(balance_split_direction(Rect::new(0.0, 0.0, 1.0, 1.5)), V);
    }

    #[test]
    fn tabs_balanced_target_is_the_largest_pane() {
        let target = SplitTarget {
            pane_id: "a".to_owned(),
            direction: V,
            place: SplitPlace::Second,
        };
        assert_eq!(pick_balanced_split_target(&three()), Some(target));
        let wide = split(V, 0.3, pane("top", None), pane("bottom", None));
        let target = SplitTarget {
            pane_id: "bottom".to_owned(),
            direction: H,
            place: SplitPlace::Second,
        };
        assert_eq!(pick_balanced_split_target(&wide), Some(target));
    }

    #[test]
    fn tabs_sibling_of_a_root_or_missing_pane_is_none() {
        assert_eq!(split_sibling_session(&pane("a", None), "a"), None);
        assert_eq!(split_sibling_session(&three(), "zz"), None);
    }

    #[test]
    fn tabs_sibling_pane_session_on_either_side() {
        let grid = split(H, 0.5, pane("a", Some("s1")), pane("b", None));
        assert_eq!(split_sibling_session(&grid, "b"), Some("s1"));
        let flipped = split(H, 0.5, pane("b", None), pane("a", Some("s1")));
        assert_eq!(split_sibling_session(&flipped, "b"), Some("s1"));
    }

    #[test]
    fn tabs_sibling_subtree_gives_its_first_session_left_to_right() {
        let sibling = split(
            V,
            0.5,
            pane("x", None),
            split(H, 0.5, pane("y", Some("s2")), pane("z", Some("s3"))),
        );
        let grid = split(H, 0.5, sibling, pane("e", None));
        assert_eq!(split_sibling_session(&grid, "e"), Some("s2"));
    }

    #[test]
    fn tabs_sibling_without_a_session_is_none() {
        let grid = split(H, 0.5, pane("a", None), pane("b", None));
        assert_eq!(split_sibling_session(&grid, "b"), None);
        let grid = split(
            H,
            0.5,
            pane("far", Some("s9")),
            split(V, 0.5, pane("x", None), pane("e", None)),
        );
        assert_eq!(
            split_sibling_session(&grid, "e"),
            None,
            "only the immediate sibling counts, not a farther pane"
        );
    }

    #[test]
    fn tabs_sibling_of_a_nested_pane() {
        assert_eq!(
            split_sibling_session(&three(), "b"),
            Some("s2"),
            "b's sibling is c, not a"
        );
        assert_eq!(split_sibling_session(&three(), "a"), Some("s2"));
    }

    #[test]
    fn tabs_placement_fills_an_empty_neighbour_of_the_same_repo() {
        let sessions = [
            session("s1", Some("r"), None),
            session("s9", Some("q"), None),
        ];
        let after = split(
            H,
            0.5,
            split(H, 0.5, pane("e1", None), pane("x", Some("s9"))),
            split(H, 0.5, pane("a", Some("s1")), pane("e2", None)),
        );
        let new = session("new", Some("r"), None);
        assert_eq!(
            pane_target_for_session(&after, &sessions, &new),
            Some(replace("e2"))
        );
        let before = split(
            H,
            0.5,
            split(H, 0.5, pane("e1", None), pane("x", Some("s9"))),
            split(H, 0.5, pane("e2", None), pane("a", Some("s1"))),
        );
        assert_eq!(
            pane_target_for_session(&before, &sessions, &new),
            Some(replace("e2"))
        );
        let in_workspace = [session("w1", Some("r"), Some("ws"))];
        let grid = split(
            H,
            0.5,
            split(H, 0.5, pane("e1", None), pane("w", Some("w1"))),
            pane("e2", None),
        );
        let new = session("new", Some("other"), Some("ws"));
        assert_eq!(
            pane_target_for_session(&grid, &in_workspace, &new),
            Some(replace("e2")),
            "a workspace session sits beside its workspace, whatever its repo"
        );
    }

    #[test]
    fn tabs_placement_splits_the_last_same_repo_pane_along_its_longer_side() {
        let sessions = [
            session("s1", Some("r"), None),
            session("s2", Some("r"), None),
            session("s9", Some("q"), None),
        ];
        let grid = split(
            H,
            0.5,
            split(H, 0.5, pane("a", Some("s1")), pane("b", Some("s2"))),
            pane("x", Some("s9")),
        );
        let new = session("new", Some("r"), None);
        assert_eq!(
            pane_target_for_session(&grid, &sessions, &new),
            Some(split_target("b", V))
        );
    }

    #[test]
    fn tabs_placement_uses_the_first_empty_pane() {
        let sessions = [session("s1", Some("r"), None)];
        let grid = split(
            H,
            0.5,
            pane("a", Some("s1")),
            split(V, 0.5, pane("e1", None), pane("e2", None)),
        );
        let new = session("new", Some("q"), None);
        assert_eq!(
            pane_target_for_session(&grid, &sessions, &new),
            Some(replace("e1"))
        );
        let no_parent = session("loose", None, None);
        assert_eq!(
            pane_target_for_session(&grid, &sessions, &no_parent),
            Some(replace("e1"))
        );
    }

    #[test]
    fn tabs_placement_splits_the_largest_pane_otherwise() {
        let sessions = [
            session("s1", Some("r"), None),
            session("s2", Some("r"), None),
        ];
        let grid = split(V, 0.3, pane("a", Some("s1")), pane("b", Some("s2")));
        let new = session("new", Some("q"), None);
        assert_eq!(
            pane_target_for_session(&grid, &sessions, &new),
            Some(split_target("b", H))
        );
    }

    #[test]
    fn tabs_placement_goes_to_the_active_tab_or_a_new_one() {
        let new = session("new", Some("q"), None);
        let empty = TabsModel::new(None);
        assert_eq!(empty.place(&new, &[]), Placement::NewTab);
        assert_eq!(
            wire(&Placement::NewTab.message("new", Some("r1".to_owned()))),
            wire(&ClientMessage::CreateTab {
                name: None,
                initial_session_id: Some("new".to_owned()),
                host: None,
                request_id: Some("r1".to_owned()),
            })
        );

        let model = model_with(&[tab("t1", &pane("e", None))]);
        let placement = model.place(&new, &[]);
        assert_eq!(
            placement,
            Placement::Pane {
                tab_id: "t1".to_owned(),
                target: replace("e"),
            }
        );
        assert_eq!(
            wire(&placement.message("new", None)),
            wire(&ClientMessage::ReplacePaneSession {
                tab_id: "t1".to_owned(),
                pane_id: "e".to_owned(),
                session_id: Some("new".to_owned()),
                host: None,
            })
        );
        let split = Placement::Pane {
            tab_id: "t1".to_owned(),
            target: split_target("e", V),
        };
        let mut split_msg = split.message("new", None);
        if let ClientMessage::SplitPane { request_id, .. } = &mut split_msg {
            assert!(
                request_id.take().is_some(),
                "a split carries an id of its own"
            );
        }
        assert_eq!(
            wire(&split_msg),
            wire(&ClientMessage::SplitPane {
                tab_id: "t1".to_owned(),
                pane_id: "e".to_owned(),
                direction: V,
                place: SplitPlace::Second,
                new_session_id: Some("new".to_owned()),
                host: None,
                request_id: None,
            })
        );

        let diff = model_with(&[diff_tab("d")]);
        assert_eq!(diff.place(&new, &[]), Placement::NewTab);
    }

    #[test]
    fn tabs_placement_in_a_named_tab_ignores_the_active_one() {
        let new = session("new", Some("q"), None);
        let model = model_with(&[
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", Some("s1"))),
        ]);
        assert_eq!(model.active_id(), Some("t1"));
        assert_eq!(
            model.place_in("t2", &new, &[session("s1", Some("r"), None)]),
            Placement::Pane {
                tab_id: "t2".to_owned(),
                target: split_target("b", H),
            }
        );
        assert_eq!(
            model.place_in("t1", &new, &[]),
            Placement::Pane {
                tab_id: "t1".to_owned(),
                target: replace("a"),
            }
        );
    }

    #[test]
    fn tabs_placement_in_a_missing_or_diff_tab_is_a_new_tab() {
        let new = session("new", Some("q"), None);
        let model = model_with(&[tab("t1", &pane("a", None)), diff_tab("d")]);
        assert_eq!(model.place_in("gone", &new, &[]), Placement::NewTab);
        assert_eq!(model.place_in("d", &new, &[]), Placement::NewTab);
    }

    #[test]
    fn place_in_a_closing_tab_is_a_new_tab() {
        let new = session("new", Some("q"), None);
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        assert_eq!(
            model.place_in("t1", &new, &[]),
            Placement::Pane {
                tab_id: "t1".to_owned(),
                target: replace("a"),
            }
        );
        model.mark_closing("t1");
        assert_eq!(model.place_in("t1", &new, &[]), Placement::NewTab);
    }

    #[test]
    fn place_with_a_closing_active_tab_is_a_new_tab() {
        let new = session("new", Some("q"), None);
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        assert_eq!(model.active_id(), Some("t1"));
        model.mark_closing("t1");
        assert_eq!(model.place(&new, &[]), Placement::NewTab);
    }

    #[test]
    fn tabs_find_tab_containing_session() {
        let tabs = [
            tab("t1", &pane("a", Some("s1"))),
            diff_tab("d"),
            tab(
                "t2",
                &split(H, 0.5, pane("b", Some("s2")), pane("c", Some("s3"))),
            ),
        ];
        assert_eq!(
            find_tab_containing_session(&tabs, "s3"),
            Some(("t2".to_owned(), "c".to_owned()))
        );
        assert_eq!(find_tab_containing_session(&tabs, "s9"), None);
    }

    #[test]
    fn tabs_focus_moves_to_a_new_split_pane() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        assert_eq!(model.take_focus_request().as_deref(), Some("a"));
        let grid = split(H, 0.5, pane("a", None), pane("n", None));
        assert!(model.apply(&updated(&tab("t1", &grid))));
        assert_eq!(model.focused_pane("t1").as_deref(), Some("n"));
        assert_eq!(model.take_focus_request().as_deref(), Some("n"));

        assert!(model.apply(&updated(&tab("t1", &pane("a", None)))));
        assert_eq!(model.focused_pane("t1").as_deref(), Some("a"));
        assert_eq!(
            model.take_focus_request().as_deref(),
            Some("a"),
            "closing the focused pane hands focus to the first"
        );
    }

    #[test]
    fn tabs_active_tab_is_kept_or_reset_on_replace() {
        let (t1, t2, t3) = (
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", None)),
            tab("t3", &pane("c", None)),
        );
        let mut model = model_with(&[t1.clone(), t2.clone()]);
        assert_eq!(model.active_id(), Some("t1"));
        model.activate("t2");
        assert_eq!(model.take_focus_request().as_deref(), Some("b"));
        model.apply(&tabs_msg(&[t1.clone(), t2.clone(), t3.clone()]));
        assert_eq!(model.active_id(), Some("t2"));
        model.apply(&tabs_msg(&[t1.clone(), t3.clone()]));
        assert_eq!(model.active_id(), Some("t1"));
        model.apply(&DaemonMessage::TabRemoved {
            tab_id: "t1".to_owned(),
        });
        assert_eq!(model.active_id(), Some("t3"));
        assert_eq!(model.take_focus_request().as_deref(), Some("c"));

        let mut restored = TabsModel::new(Some("t3".to_owned()));
        restored.apply(&tabs_msg(&[t1.clone(), t2.clone(), t3.clone()]));
        assert_eq!(restored.active_id(), Some("t3"));
        let mut gone = TabsModel::new(Some("t9".to_owned()));
        gone.apply(&tabs_msg(&[t1, t2, t3]));
        assert_eq!(gone.active_id(), Some("t1"));
    }

    #[test]
    fn a_closing_tab_is_forgotten_when_it_is_removed() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.mark_closing("t1");
        assert!(model.closing().contains("t1"));

        model.apply(&updated(&tab("t1", &pane("c", None))));
        assert!(model.closing().contains("t1"), "an update clears no mark");

        assert!(model.apply(&DaemonMessage::TabRemoved {
            tab_id: "t1".to_owned(),
        }));
        assert!(!model.closing().contains("t1"));
        assert!(model.closing().is_empty());
    }

    #[test]
    fn a_full_list_drops_closing_marks() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.mark_closing("t1");
        model.mark_closing("t2");

        assert!(model.apply(&tabs_msg(&[
            tab("t1", &pane("a", None)),
            tab("t3", &pane("c", None)),
        ])));
        assert!(
            model.closing().contains("t1"),
            "still listed: its close may still be on its way"
        );
        assert!(
            !model.closing().contains("t2"),
            "gone from the list: nothing waits"
        );
    }

    #[test]
    fn reset_clears_closing_marks() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.mark_closing("t1");

        model.clear_pending_restore();
        assert!(
            model.closing().is_empty(),
            "a fresh connection starts clean"
        );
    }

    fn split_request(tab_id: &str, request_id: Option<&str>) -> ClientMessage {
        ClientMessage::SplitPane {
            tab_id: tab_id.to_owned(),
            pane_id: "a".to_owned(),
            direction: V,
            place: SplitPlace::Second,
            new_session_id: None,
            host: None,
            request_id: request_id.map(str::to_owned),
        }
    }

    fn move_request(src: (&str, &str), dst: (&str, &str), request_id: &str) -> ClientMessage {
        ClientMessage::MovePane {
            src_tab_id: src.0.to_owned(),
            src_pane_id: src.1.to_owned(),
            dst_tab_id: dst.0.to_owned(),
            dst_pane_id: dst.1.to_owned(),
            edge: PaneDropEdge::Right,
            touch: false,
            request_id: Some(request_id.to_owned()),
        }
    }

    /// Whether `tab_id` can be marked closing now; marks it if so.
    fn marks(model: &mut TabsModel, tab_id: &str) -> bool {
        model.mark_closing_if_last_pane(tab_id, None);
        model.closing().contains(tab_id)
    }

    #[test]
    fn a_failed_move_clears_the_closing_mark() {
        let mut model = model_with(&[
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", None)),
            tab("t3", &split(H, 0.5, pane("c", None), pane("d", None))),
        ]);
        model.mark_closing_if_last_pane("t1", Some("m1"));
        model.note_layout_request(&move_request(("t1", "a"), ("t2", "b"), "m1"));
        assert!(model.closing().contains("t1"));
        model.fail_layout_request("m2");
        assert!(model.closing().contains("t1"), "another request's refusal");
        model.fail_layout_request("m1");
        assert!(!model.closing().contains("t1"), "the move was refused");

        model.mark_closing("t2");
        model.mark_closing_if_last_pane("t2", Some("m3"));
        model.fail_layout_request("m3");
        assert!(model.closing().contains("t2"), "a close's mark stays");

        let some_panes = ["c".to_owned()];
        model.mark_closing_if_emptied("t3", &some_panes, Some("x1"));
        assert!(!model.closing().contains("t3"), "a pane stays behind");
        let every_pane = ["c".to_owned(), "d".to_owned()];
        model.mark_closing_if_emptied("t3", &every_pane, Some("x1"));
        assert!(model.closing().contains("t3"), "every pane leaves");
        model.fail_layout_request("x1");
        assert!(!model.closing().contains("t3"), "the extract was refused");
    }

    #[test]
    fn a_mark_is_not_set_while_a_split_is_in_flight() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.note_layout_request(&split_request("t1", Some("s1")));
        assert!(!marks(&mut model, "t1"), "the split may keep the tab");
    }

    #[test]
    fn a_move_in_flight_suppresses_the_mark_on_both_tabs() {
        let mut model = model_with(&[
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", None)),
            tab("t3", &pane("c", None)),
        ]);
        model.note_layout_request(&move_request(("t1", "a"), ("t2", "b"), "m1"));
        assert!(!marks(&mut model, "t1"), "the source");
        assert!(!marks(&mut model, "t2"), "the destination");
        model.note_layout_request(&move_request(("t3", "c"), ("t3", "c"), "m2"));
        assert!(marks(&mut model, "t3"), "a move onto itself is no request");
    }

    fn swap_request(src: (&str, &str), dst: (&str, &str), request_id: &str) -> ClientMessage {
        ClientMessage::SwapPanes {
            src_tab_id: src.0.to_owned(),
            src_pane_id: src.1.to_owned(),
            dst_tab_id: dst.0.to_owned(),
            dst_pane_id: dst.1.to_owned(),
            request_id: Some(request_id.to_owned()),
        }
    }

    #[test]
    fn a_swap_in_flight_marks_both_tabs_and_a_failed_swap_clears_it() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.note_layout_request(&swap_request(("t1", "a"), ("t2", "b"), "w1"));
        assert!(!marks(&mut model, "t1"), "the source");
        assert!(!marks(&mut model, "t2"), "the destination");
        model.fail_layout_request("w1");
        assert!(marks(&mut model, "t1"), "the refused swap changes nothing");
        assert!(marks(&mut model, "t2"));
    }

    #[test]
    fn a_tab_update_ends_a_layout_request_in_flight() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.note_layout_request(&split_request("t1", Some("s1")));
        model.note_layout_request(&split_request("t1", None));
        model.apply(&updated(&tab("t1", &pane("a", None))));
        assert!(!marks(&mut model, "t1"), "one request is still on its way");
        model.apply(&updated(&tab("t1", &pane("a", None))));
        assert!(marks(&mut model, "t1"), "both are answered");
    }

    #[test]
    fn a_tab_removed_ends_all_requests_for_the_tab() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.note_layout_request(&split_request("t1", Some("s1")));
        model.note_layout_request(&split_request("t1", Some("s2")));
        model.note_layout_request(&move_request(("t1", "a"), ("t2", "b"), "m1"));
        model.apply(&DaemonMessage::TabRemoved {
            tab_id: "t1".to_owned(),
        });
        model.apply(&updated(&tab("t1", &pane("a", None))));
        assert!(marks(&mut model, "t1"), "nothing waits on the tab");
        assert!(!marks(&mut model, "t2"), "the move's other tab still waits");
    }

    #[test]
    fn a_failed_split_ends_its_request_in_flight() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.note_layout_request(&split_request("t1", Some("s1")));
        model.fail_layout_request("s1");
        assert!(marks(&mut model, "t1"), "the refused split changes nothing");
    }

    #[test]
    fn a_restore_snapshot_in_flight_suppresses_the_mark() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.note_layout_request(&ClientMessage::RestoreTabSnapshot {
            tab: tab("t1", &split(H, 0.5, pane("a", None), pane("b", None))),
        });
        assert!(!marks(&mut model, "t1"), "the restore may bring panes back");
        model.apply(&updated(&tab("t1", &pane("a", None))));
        assert!(marks(&mut model, "t1"), "the restore is answered");
    }

    #[test]
    fn a_reconnect_clears_in_flight_layout_requests() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.note_layout_request(&split_request("t1", Some("s1")));
        model.clear_layout_requests();
        assert!(
            marks(&mut model, "t1"),
            "a fresh connection answers none of the old one's requests"
        );
    }

    #[test]
    fn tabs_reorder_follows_the_daemon() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: vec!["t2".to_owned(), "t1".to_owned()],
        });
        let ids: Vec<&str> = model.tabs().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["t2", "t1"]);
    }

    fn strip_order(model: &TabsModel) -> Vec<&str> {
        model.tabs().iter().map(|t| t.id.as_str()).collect()
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn reorder_local_applies_a_permutation_and_keeps_the_active_tab() {
        let mut model = model_with(&[
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", None)),
            tab("t3", &pane("c", None)),
        ]);
        model.activate("t2");
        model
            .selection
            .click(&["t1", "t2", "t3"], Some("t2"), "t3", PillClick::Toggle);

        assert!(model.reorder_local(&ids(&["t3", "t1", "t2"])));

        assert_eq!(strip_order(&model), ["t3", "t1", "t2"]);
        assert_eq!(model.active_id(), Some("t2"), "the shown tab stays");
        assert!(model.selection.contains("t3"), "the selection stays");
    }

    #[test]
    fn reorder_local_refuses_a_list_that_is_not_a_permutation() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        for wrong in [
            ids(&["t2"]),
            ids(&["t2", "t1", "t9"]),
            ids(&["t2", "t2"]),
            ids(&["t2", "t9"]),
            Vec::new(),
        ] {
            assert!(!model.reorder_local(&wrong), "{wrong:?} is refused");
            assert_eq!(
                strip_order(&model),
                ["t1", "t2"],
                "{wrong:?} changes nothing"
            );
        }
    }

    /// `t1`, `t2` and `t3`, each one empty pane, in that order.
    fn three_strip() -> TabsModel {
        model_with(&[
            tab("t1", &pane("a", None)),
            tab("t2", &pane("b", None)),
            tab("t3", &pane("c", None)),
        ])
    }

    #[test]
    fn a_refused_reorder_restores_the_previous_order() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t3", "t1"]));
        assert!(model.reorder_local(&ids(&["t2", "t3", "t1"])));

        model.fail_reorder("r1");

        assert_eq!(strip_order(&model), ["t1", "t2", "t3"]);
    }

    #[test]
    fn two_refused_reorders_restore_the_daemons_order() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t1", "t3"]));
        assert!(model.reorder_local(&ids(&["t2", "t1", "t3"])));
        model.note_reorder("r2", &ids(&["t3", "t2", "t1"]));
        assert!(model.reorder_local(&ids(&["t3", "t2", "t1"])));

        model.fail_reorder("r1");

        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "r2 is still pending, so the refused r1 leaves the strip"
        );

        model.fail_reorder("r2");

        assert_eq!(
            strip_order(&model),
            ["t1", "t2", "t3"],
            "the refusal hands its order on, so the last one goes back to the daemon's"
        );
    }

    #[test]
    fn an_echo_settles_the_pending_reorder_it_answers() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t1", "t3"]));
        assert!(model.reorder_local(&ids(&["t2", "t1", "t3"])));
        model.note_reorder("r2", &ids(&["t3", "t2", "t1"]));
        assert!(model.reorder_local(&ids(&["t3", "t2", "t1"])));

        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: ids(&["t3", "t2", "t1"]),
        });

        assert_eq!(strip_order(&model), ["t3", "t2", "t1"]);
        model.fail_reorder("r2");
        model.fail_reorder("r1");
        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "the echo settled r2 and dropped the older r1 with it"
        );
    }

    #[test]
    fn a_restore_echo_settles_no_pending_reorder() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t3", "t1"]));
        assert!(model.reorder_local(&ids(&["t2", "t3", "t1"])));

        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: ids(&["t3", "t1", "t2"]),
        });

        assert_eq!(
            strip_order(&model),
            ["t3", "t1", "t2"],
            "an echo r1 never sent is applied as it comes"
        );

        model.fail_reorder("r1");

        assert_eq!(
            strip_order(&model),
            ["t1", "t2", "t3"],
            "r1 is still pending, so its refusal snaps back"
        );
    }

    #[test]
    fn an_older_echo_does_not_move_the_strip_while_a_newer_reorder_is_pending() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t1", "t3"]));
        assert!(model.reorder_local(&ids(&["t2", "t1", "t3"])));
        model.note_reorder("r2", &ids(&["t3", "t2", "t1"]));
        assert!(model.reorder_local(&ids(&["t3", "t2", "t1"])));

        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: ids(&["t2", "t1", "t3"]),
        });

        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "r1's echo must not pull the strip back under the pending r2"
        );

        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: ids(&["t3", "t2", "t1"]),
        });

        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "the last reorder's echo is the strip's order"
        );
    }

    #[test]
    fn a_refusal_with_another_reorder_pending_leaves_the_strip() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t1", "t3"]));
        assert!(model.reorder_local(&ids(&["t2", "t1", "t3"])));
        model.note_reorder("r2", &ids(&["t3", "t2", "t1"]));
        assert!(model.reorder_local(&ids(&["t3", "t2", "t1"])));

        model.fail_reorder("r1");

        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "r2 is still pending, so the refused r1 leaves the strip"
        );

        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: ids(&["t3", "t2", "t1"]),
        });
        model.fail_reorder("r2");

        assert_eq!(
            strip_order(&model),
            ["t3", "t2", "t1"],
            "r2's echo settled it, so refusing it changes nothing"
        );
    }

    #[test]
    fn a_reconnect_forgets_pending_reorders() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t3", "t1"]));
        assert!(model.reorder_local(&ids(&["t2", "t3", "t1"])));

        model.clear_layout_requests();
        model.fail_reorder("r1");

        assert_eq!(
            strip_order(&model),
            ["t2", "t3", "t1"],
            "a fresh connection answers none of the old one's requests"
        );
    }

    #[test]
    fn a_fresh_tab_list_forgets_pending_reorders() {
        let mut model = three_strip();
        let wanted = ids(&["t2", "t3", "t1"]);
        model.note_reorder("r1", &wanted);
        assert!(model.reorder_local(&wanted));
        let fresh: Vec<TabEntry> = wanted.iter().map(|id| tab(id, &pane("p", None))).collect();

        assert!(model.apply(&tabs_msg(&fresh)));
        model.fail_reorder("r1");

        assert_eq!(strip_order(&model), ["t2", "t3", "t1"]);
    }

    #[test]
    fn a_snap_back_keeps_a_tab_created_since_the_drop() {
        let mut model = three_strip();
        model.note_reorder("r1", &ids(&["t2", "t3", "t1"]));
        assert!(model.reorder_local(&ids(&["t2", "t3", "t1"])));
        model.apply(&updated(&tab("t4", &pane("d", None))));

        model.fail_reorder("r1");

        assert_eq!(
            strip_order(&model),
            ["t1", "t2", "t3", "t4"],
            "the new tab comes after the restored ones"
        );
    }

    #[test]
    fn tabs_a_requested_tab_becomes_active_once() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(model.active_id(), Some("t1"), "another client's tab");
        model.arm_create("r1");
        model.apply(&updated(&tab("t3", &pane("c", None))));
        assert_eq!(model.active_id(), Some("t3"));
        assert_eq!(model.take_focus_request().as_deref(), Some("c"));
        model.apply(&updated(&tab("t4", &pane("d", None))));
        assert_eq!(model.active_id(), Some("t3"), "the arm is spent");

        let mut none_active = TabsModel::new(None);
        none_active.apply(&updated(&tab("t1", &pane("a", None))));
        assert_eq!(none_active.active_id(), Some("t1"));
    }

    #[test]
    fn a_rejected_create_is_disarmed() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.arm_create("r1");
        model.disarm_create("r1");
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(model.active_id(), Some("t1"), "another client's tab");
    }

    #[test]
    fn disarming_one_create_keeps_the_others_armed() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.arm_create("r1");
        model.arm_create("r2");
        model.disarm_create("r1");
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(model.active_id(), Some("t2"), "r2 is still armed");
        model.apply(&updated(&tab("t3", &pane("c", None))));
        assert_eq!(model.active_id(), Some("t2"), "and now spent");
    }

    #[test]
    fn disarming_an_id_already_consumed_changes_nothing() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.arm_create("r1");
        model.arm_create("r2");
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(model.active_id(), Some("t2"), "the oldest arm is spent");
        model.disarm_create("r1");
        model.apply(&updated(&tab("t3", &pane("c", None))));
        assert_eq!(model.active_id(), Some("t3"), "r2 is still armed");
        model.apply(&updated(&tab("t4", &pane("d", None))));
        assert_eq!(model.active_id(), Some("t3"), "nothing is left armed");
    }

    #[test]
    fn a_reconnect_clears_armed_creates() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.arm_create("r1");
        model.clear_armed_creates();
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(
            model.active_id(),
            Some("t1"),
            "the old connection's arm is gone"
        );
    }

    #[test]
    fn tabs_view_counts_track_panes_showing_a_session() {
        let two = split(H, 0.5, pane("a", Some("s1")), pane("b", Some("s1")));
        let mut model = model_with(&[tab("t1", &two), tab("t2", &pane("c", Some("s2")))]);
        let before = model.view_counts();
        assert_eq!(before.get("s1"), Some(&2));
        assert_eq!(before.get("s2"), Some(&1));

        model.apply(&updated(&tab("t1", &pane("a", Some("s1")))));
        let after = model.view_counts();
        assert_eq!(after.get("s1"), Some(&1));
        assert_eq!(
            session_changes(&before, &after),
            SessionChanges {
                detach: Vec::new(),
                resync: vec!["s1".to_owned()],
            }
        );

        let before = after;
        model.apply(&DaemonMessage::TabRemoved {
            tab_id: "t1".to_owned(),
        });
        let after = model.view_counts();
        assert_eq!(after.get("s1"), None);
        assert_eq!(
            session_changes(&before, &after),
            SessionChanges {
                detach: vec!["s1".to_owned()],
                resync: Vec::new(),
            }
        );
        let bindings = model.bindings();
        assert_eq!(
            bindings,
            vec![PaneBinding {
                tab_id: "t2".to_owned(),
                pane_id: "c".to_owned(),
                session_id: Some("s2".to_owned()),
            }]
        );
    }

    #[test]
    fn tabs_close_confirm_arms_and_disarms() {
        let trivial = tab("t0", &pane("a", None));
        let bound = tab("t1", &pane("a", Some("s1")));
        let two_panes = tab("t2", &split(H, 0.5, pane("a", None), pane("b", None)));
        let mut confirm = CloseConfirm::default();
        assert!(confirm.click(&trivial), "nothing to lose closes at once");
        assert!(!confirm.click(&bound), "a bound session arms first");
        assert_eq!(confirm.armed(), Some("t1"));
        assert!(
            !confirm.click(&two_panes),
            "arming another tab moves the arm"
        );
        assert_eq!(confirm.armed(), Some("t2"));
        assert!(confirm.click(&two_panes));
        assert_eq!(confirm.armed(), None);
        assert!(!confirm.click(&bound));
        assert!(confirm.disarm());
        assert!(!confirm.disarm());
        assert!(!confirm.click(&bound), "a disarmed tab arms again");
        assert!(!needs_confirm(&diff_tab("d")));
    }

    #[test]
    fn tabs_ui_state_keeps_the_active_tab() {
        let state = UiState {
            active_tab_id: Some("t2".to_owned()),
            ..UiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
        let older: UiState = serde_json::from_str(
            r#"{ "sidebar_width": 300.0, "sidebar_collapsed": true, "collapsed_containers": [] }"#,
        )
        .expect("a file from before the active tab was saved");
        assert_eq!(older.active_tab_id, None);
        assert!(older.sidebar_collapsed);
    }

    fn with_status(id: &str, status: SessionStatus) -> SessionSnapshot {
        let mut s = session(id, None, None);
        s.status = status;
        s
    }

    #[test]
    fn tab_counts_busy_among_live_panes() {
        let grid = split(
            H,
            0.5,
            pane("a", Some("work")),
            split(V, 0.5, pane("b", Some("idle")), pane("c", Some("ask"))),
        );
        let sessions = [
            with_status("work", SessionStatus::Working),
            with_status("idle", SessionStatus::Idle),
            with_status("ask", SessionStatus::AwaitingInput),
        ];
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &sessions),
            Some((2, 3))
        );
    }

    #[test]
    fn tab_counts_skip_inactive_unknown_and_empty_panes() {
        let grid = split(
            H,
            0.5,
            split(V, 0.5, pane("a", Some("gone")), pane("b", None)),
            split(V, 0.5, pane("c", Some("spawn")), pane("d", Some("lost"))),
        );
        let mut gone = with_status("gone", SessionStatus::Working);
        gone.is_inactive = true;
        let sessions = [gone, with_status("spawn", SessionStatus::Spawning)];
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &sessions),
            Some((1, 1)),
            "an inactive session, an empty pane and an unlisted session do not count"
        );
    }

    #[test]
    fn tab_counts_a_session_in_two_panes_once_per_pane() {
        let grid = split(H, 0.5, pane("a", Some("s")), pane("b", Some("s")));
        let sessions = [with_status("s", SessionStatus::Working)];
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &sessions),
            Some((2, 2))
        );
    }

    #[test]
    fn tab_counts_leave_an_excluded_busy_session_out_of_busy() {
        let grid = split(H, 0.5, pane("a", Some("server")), pane("b", Some("work")));
        let mut server = with_status("server", SessionStatus::Working);
        server.busy_excluded = true;
        let sessions = [server, with_status("work", SessionStatus::Working)];
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &sessions),
            Some((1, 2)),
            "the excluded working session leaves busy"
        );
    }

    #[test]
    fn tab_counts_still_count_an_excluded_session_in_total() {
        let grid = split(H, 0.5, pane("a", Some("ask")), pane("b", Some("idle")));
        let mut ask = with_status("ask", SessionStatus::AwaitingInput);
        ask.busy_excluded = true;
        let mut idle = with_status("idle", SessionStatus::Idle);
        idle.busy_excluded = true;
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &[ask, idle]),
            Some((0, 2)),
            "excluded sessions stay in total whatever their status"
        );
    }

    #[test]
    fn a_background_session_counts_as_busy_on_the_pill() {
        let grid = split(
            H,
            0.5,
            pane("a", Some("bg")),
            split(V, 0.5, pane("b", Some("idle")), pane("c", Some("server"))),
        );
        let mut bg = with_status("bg", SessionStatus::Idle);
        bg.background = true;
        let mut server = with_status("server", SessionStatus::Idle);
        server.background = true;
        server.busy_excluded = true;
        let sessions = [bg, with_status("idle", SessionStatus::Idle), server];
        assert_eq!(
            tab_session_counts(&tab("t", &grid), &sessions),
            Some((1, 3)),
            "idle with background work is busy, unless marked not to count"
        );
    }

    #[test]
    fn a_diff_tab_has_no_counts() {
        assert_eq!(tab_session_counts(&diff_tab("d"), &[]), None);
    }

    const STRIP: [&str; 4] = ["a", "b", "c", "d"];

    #[test]
    fn selection_ctrl_toggles_without_activating() {
        let mut selection = TabSelection::default();
        assert!(!selection.click(&STRIP, None, "b", PillClick::Toggle));
        assert!(!selection.click(&STRIP, None, "d", PillClick::Toggle));
        assert!(selection.contains("b") && selection.contains("d"));
        assert_eq!(selection.anchor(), Some("d"));
        assert!(!selection.click(&STRIP, None, "b", PillClick::Toggle));
        assert!(!selection.contains("b"), "a second Ctrl+click deselects");
        assert_eq!(selection.len(), 1);
        assert_eq!(selection.anchor(), Some("b"));
    }

    #[test]
    fn shift_selects_range_from_anchor() {
        let mut selection = TabSelection::default();
        selection.click(&STRIP, None, "c", PillClick::Plain);
        assert!(!selection.click(&STRIP, None, "a", PillClick::Range));
        let picked: Vec<&str> = STRIP
            .into_iter()
            .filter(|id| selection.contains(id))
            .collect();
        assert_eq!(picked, ["a", "b", "c"]);
        assert_eq!(selection.anchor(), Some("c"), "the anchor stays");
        selection.click(&STRIP, None, "d", PillClick::Range);
        assert!(!selection.contains("a") && selection.contains("c") && selection.contains("d"));
    }

    #[test]
    fn shift_without_anchor_is_plain_click() {
        let mut selection = TabSelection::default();
        assert!(selection.click(&STRIP, None, "b", PillClick::Range));
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), Some("b"));
    }

    #[test]
    fn shift_ranges_from_active_after_keyboard_switch() {
        let mut selection = TabSelection::default();
        assert!(selection.click(&STRIP, Some("a"), "a", PillClick::Plain));
        assert!(!selection.click(&STRIP, Some("c"), "d", PillClick::Range));
        let picked: Vec<&str> = STRIP
            .into_iter()
            .filter(|id| selection.contains(id))
            .collect();
        assert_eq!(
            picked,
            ["c", "d"],
            "a shortcut showed c after the click on a"
        );
        assert_eq!(selection.anchor(), Some("c"));

        selection.click(&STRIP, Some("c"), "b", PillClick::Toggle);
        selection.click(&STRIP, Some("c"), "a", PillClick::Range);
        let picked: Vec<&str> = STRIP
            .into_iter()
            .filter(|id| selection.contains(id))
            .collect();
        assert_eq!(
            picked,
            ["a", "b"],
            "a Ctrl+click anchor holds while c stays shown"
        );
    }

    #[test]
    fn plain_click_clears_selection() {
        let mut selection = TabSelection::default();
        selection.click(&STRIP, None, "a", PillClick::Toggle);
        selection.click(&STRIP, None, "b", PillClick::Toggle);
        assert!(selection.click(&STRIP, None, "c", PillClick::Plain));
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), Some("c"));
    }

    #[test]
    fn selection_prunes_removed_tabs() {
        let tabs: Vec<TabEntry> = STRIP.iter().map(|id| tab(id, &pane(id, None))).collect();
        let mut model = model_with(&tabs);
        model.selection.click(&STRIP, None, "a", PillClick::Toggle);
        model.selection.click(&STRIP, None, "b", PillClick::Toggle);
        model.selection.click(&STRIP, None, "c", PillClick::Toggle);
        assert!(model.apply(&DaemonMessage::TabRemoved {
            tab_id: "c".to_owned()
        }));
        assert!(!model.selection.contains("c"));
        assert_eq!(model.selection.anchor(), None, "the anchor went with it");
        assert!(model.apply(&tabs_msg(&[tabs[0].clone(), tabs[3].clone()])));
        assert!(model.selection.contains("a"));
        assert!(!model.selection.contains("b"), "a list without b drops it");
        assert_eq!(model.selection.len(), 1);
    }

    #[test]
    fn bound_pane_count_skips_empty() {
        assert_eq!(bound_pane_count(&three()), 2);
        assert_eq!(bound_pane_count(&pane("p", None)), 0);
    }

    #[test]
    fn tab_pill_is_unbound_one_or_many_panes() {
        let mut work = tab("t1", &pane("p1", Some("s1")));
        "work".clone_into(&mut work.name);
        let other = tab(
            "t2",
            &split(H, 0.5, pane("p2", Some("s2")), pane("p3", None)),
        );
        let also = tab("t3", &pane("p4", Some("s2")));
        let tabs = [work, other, also, diff_tab("d")];
        let pills = tab_pills(&tabs);
        assert_eq!(pills.get("s9"), None, "unbound");
        assert_eq!(
            pills.get("s1"),
            Some(&TabPill::One {
                tab_id: "t1".to_owned(),
                name: "work".to_owned(),
            })
        );
        assert_eq!(
            pills.get("s2"),
            Some(&TabPill::Many(vec!["t2".to_owned(), "t3".to_owned()]))
        );
        assert_eq!(pills.len(), 2, "empty panes add nothing");
    }

    #[test]
    fn tab_pill_counts_two_panes_of_one_tab_as_two() {
        let grid = split(H, 0.5, pane("a", Some("s")), pane("b", Some("s")));
        assert_eq!(
            tab_pills(&[tab("t", &grid)]).get("s"),
            Some(&TabPill::Many(vec!["t".to_owned(), "t".to_owned()]))
        );
    }

    #[test]
    fn drop_target_prefers_empty_pane_replace() {
        assert_eq!(
            pick_balanced_drop_target(&three()),
            Some(("b".to_owned(), PaneDropEdge::Replace)),
            "the empty pane takes it, though `a` is larger"
        );
    }

    #[test]
    fn tab_pill_hover_counts_panes_and_names_each_tab_once() {
        let many = TabPill::Many(vec!["a".to_owned(), "a".to_owned(), "b".to_owned()]);
        assert_eq!(many.hover(), "Open in 3 panes: a, b");
        let one = TabPill::One {
            tab_id: "t1".to_owned(),
            name: "work".to_owned(),
        };
        assert_eq!(one.hover(), "Open in tab \"work\"");
        assert_eq!(
            TabPill::Unbound.hover(),
            "No tab references this session — click to open it in a new tab"
        );
    }

    #[test]
    fn drop_target_splits_the_largest_pane_right_or_bottom() {
        let full = split(H, 0.5, pane("a", Some("s1")), pane("c", Some("s2")));
        assert_eq!(
            pick_balanced_drop_target(&full),
            Some(("a".to_owned(), PaneDropEdge::Bottom)),
            "a tall half splits along its height"
        );
        let stacked = split(V, 0.3, pane("top", Some("s1")), pane("bottom", Some("s2")));
        assert_eq!(
            pick_balanced_drop_target(&stacked),
            Some(("bottom".to_owned(), PaneDropEdge::Right)),
            "a wide pane splits along its width"
        );
    }

    #[test]
    fn restored_tab_is_activated_when_its_id_arrives() {
        let mut model = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        assert_eq!(model.active_id(), Some("t1"));
        model.activate_on_arrival("t9", Some("p9"));

        model.apply(&updated(&tab("t2", &pane("p2", None))));
        assert_eq!(model.active_id(), Some("t1"), "another tab changes nothing");

        model.apply(&updated(&tab("t9", &pane("p9", Some("s2")))));
        assert_eq!(model.active_id(), Some("t9"), "the named id activates it");
        assert_eq!(
            model.focused_pane("t9").as_deref(),
            Some("p9"),
            "and the pane it was to focus"
        );
    }

    #[test]
    fn other_tab_arriving_is_not_activated_by_a_pending_restore() {
        let mut model = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        model.activate_on_arrival("t9", None);

        model.apply(&updated(&tab("t2", &pane("p2", None))));
        assert_eq!(model.active_id(), Some("t1"), "the tab shown is unchanged");
        assert!(
            model.pending_restore.contains_key("t9"),
            "and the restore still waits for its own id"
        );
    }

    #[test]
    fn pending_restore_dropped_when_tab_list_lacks_it() {
        let mut model = model_with(&[tab("t1", &pane("p1", Some("s1")))]);
        model.activate_on_arrival("t9", Some("p9"));

        model.apply(&tabs_msg(&[tab("t1", &pane("p1", Some("s1")))]));
        assert!(
            !model.pending_restore.contains_key("t9"),
            "the list has no t9, so nothing is coming for it"
        );

        model.apply(&updated(&tab("t9", &pane("p9", Some("s2")))));
        assert_eq!(
            model.active_id(),
            Some("t1"),
            "a tab of that id later shows nothing new"
        );
    }

    fn auto_grid(aspect: Option<f32>) -> RearrangeLayout {
        RearrangeLayout::Grid { cols: 0, aspect }
    }

    /// A tab whose pick is `layout`.
    fn picked_tab(id: &str, layout: RearrangeLayout) -> TabEntry {
        let mut tab = tab(id, &pane("p1", None));
        tab.picked_layout = Some(layout);
        tab
    }

    #[test]
    fn auto_grid_resends_follow_the_aspect_and_the_pick() {
        let wide = 2.0;
        assert!(
            auto_grid_resends(&[picked_tab("a", auto_grid(Some(wide)))], wide).is_empty(),
            "the stored aspect is the pane area's: nothing to re-pick"
        );
        assert!(
            auto_grid_resends(&[picked_tab("a", auto_grid(Some(wide - 0.005)))], wide).is_empty(),
            "a wobble under the epsilon is not a resize"
        );
        assert_eq!(
            auto_grid_resends(&[picked_tab("a", auto_grid(Some(1.0)))], wide),
            vec![("a".to_owned(), auto_grid(Some(wide)))],
            "a moved aspect re-picks"
        );
        assert_eq!(
            auto_grid_resends(&[picked_tab("a", auto_grid(None))], wide),
            vec![("a".to_owned(), auto_grid(Some(wide)))],
            "a pick with no aspect re-picks once the area is known"
        );
        assert!(
            auto_grid_resends(
                &[picked_tab(
                    "a",
                    RearrangeLayout::Grid {
                        cols: 2,
                        aspect: None
                    }
                )],
                wide
            )
            .is_empty(),
            "a fixed count is not touched by a resize"
        );
        assert!(
            auto_grid_resends(&[picked_tab("a", RearrangeLayout::Horizontal)], wide).is_empty(),
            "another layout is not touched either"
        );
    }

    #[test]
    fn auto_grid_resends_come_in_tab_order_and_skip_the_rest() {
        let wide = 2.0;
        let tabs = vec![
            picked_tab("a", auto_grid(Some(1.0))),
            tab("b", &pane("p1", None)),
            picked_tab(
                "c",
                RearrangeLayout::Grid {
                    cols: 3,
                    aspect: None,
                },
            ),
            diff_tab("d"),
            picked_tab("e", auto_grid(None)),
        ];
        assert_eq!(
            auto_grid_resends(&tabs, wide),
            vec![
                ("a".to_owned(), auto_grid(Some(wide))),
                ("e".to_owned(), auto_grid(Some(wide))),
            ],
            "the two automatic tabs, in order; never-picked, fixed, and diff tabs stay"
        );
    }
}
