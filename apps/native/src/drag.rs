//! The plain-Rust half of drag and drop: what a drag carries, the edge of a
//! pane a pointer points at, the message a pane drop sends, and where a row or
//! pill reorder puts the dragged item, the overlay a hovered pane draws, the
//! label a dragged pane's ghost shows, what a pane over a pill does,
//! whether a pane drag still has a pane and a session to end at, and what a
//! sidebar leaf dropped on a pane or a pill does.

use protocol::{
    ClientMessage, ContainerRef, GridNode, PaneDropEdge, SessionSnapshot, SplitDirection,
    SplitPlace, TabEntry,
};

use crate::sidebar::{Container, ContainerKind, can_attach};
use crate::tabs::{self, OuterSides, PaneTarget, Placement, Rect, TabsModel};
use crate::undo;

/// How much of a pane's side that touches the grid's edge is its outer band.
pub const OUTER_BAND: f32 = 0.12;
/// The centre box's lower bound on both axes of a pane.
pub const CENTRE_LOW: f32 = 0.35;
/// The centre box's upper bound on both axes of a pane.
pub const CENTRE_HIGH: f32 = 0.65;
/// How far an outer strip's overlay reaches into the pane, as a share of the
/// pane across that side.
pub const OUTER_STRIP: f32 = 0.14;
/// How far the centre overlay sits in from each side of the pane, as a share
/// of the pane along that axis.
pub const CENTRE_INSET: f32 = 0.12;
/// The badge in the centre overlay.
pub const SWAP_BADGE: &str = "Swap panes";
/// What the header of a pane that shows no session says, and its ghost.
pub const EMPTY_PANE_LABEL: &str = "Empty pane";

/// What a drag carries: the item the pointer picked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragPayload {
    /// A pane, dragged by its header.
    Pane { tab_id: String, pane_id: String },
    /// A tab's pill, or its container in the sidebar's tabs view.
    Tab(String),
    /// A sidebar container: a repo or a workspace.
    Container(ContainerRef),
    /// A sidebar leaf: its session and the container holding it.
    Leaf {
        session_id: String,
        container_id: String,
    },
}

/// Which half of a row or pill a pointer points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

/// A pane move, a drop's or a menu's: the pane, where it goes, and the undo
/// label it records.
#[derive(Debug, Clone)]
pub struct PaneDrop {
    pub src_tab_id: String,
    pub src_pane_id: String,
    pub dst_tab_id: String,
    pub dst_pane_id: String,
    pub edge: PaneDropEdge,
    /// Whether the destination tab counts as arranged by hand from then on: a
    /// drop on a pane does, a pill drop or the menu's "Move to ▸ tab" does not.
    pub touch: bool,
    /// Whether the two panes exchange their sessions instead of one moving.
    pub swap: bool,
    pub request_id: Option<String>,
    pub undo_label: &'static str,
}

impl PaneDrop {
    /// The [`ClientMessage::SwapPanes`] a swap sends, or the
    /// [`ClientMessage::MovePane`] a move sends.
    #[must_use]
    pub fn message(&self) -> ClientMessage {
        if self.swap {
            return ClientMessage::SwapPanes {
                src_tab_id: self.src_tab_id.clone(),
                src_pane_id: self.src_pane_id.clone(),
                dst_tab_id: self.dst_tab_id.clone(),
                dst_pane_id: self.dst_pane_id.clone(),
                request_id: self.request_id.clone(),
            };
        }
        ClientMessage::MovePane {
            src_tab_id: self.src_tab_id.clone(),
            src_pane_id: self.src_pane_id.clone(),
            dst_tab_id: self.dst_tab_id.clone(),
            dst_pane_id: self.dst_pane_id.clone(),
            edge: self.edge,
            touch: self.touch,
            request_id: self.request_id.clone(),
        }
    }
}

/// What part of a pane a drop overlay tints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// A half of the pane: the dropped pane splits it.
    Half,
    /// A strip along the grid's edge: the dropped pane splits the whole grid.
    Outer,
    /// The centre box: the two panes swap.
    Centre,
}

/// The overlay a pane draws while a drop would land on `edge` of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropOverlay {
    pub kind: OverlayKind,
    /// Where the tint goes, in the units of the pane it was asked for.
    pub rect: Rect,
}

/// The overlay for a drop on `edge` of `pane`: that half, a strip
/// [`OUTER_STRIP`] deep along an outer side, or the centre inset by
/// [`CENTRE_INSET`] on every side.
#[must_use]
pub fn drop_overlay(edge: PaneDropEdge, pane: Rect) -> DropOverlay {
    let Rect {
        x,
        y,
        width: w,
        height: h,
    } = pane;
    let (strip_w, strip_h) = (w * OUTER_STRIP, h * OUTER_STRIP);
    let (inset_w, inset_h) = (w * CENTRE_INSET, h * CENTRE_INSET);
    let (kind, rect) = match edge {
        PaneDropEdge::Left => (OverlayKind::Half, Rect::new(x, y, w / 2.0, h)),
        PaneDropEdge::Right => (OverlayKind::Half, Rect::new(x + w / 2.0, y, w / 2.0, h)),
        PaneDropEdge::Top => (OverlayKind::Half, Rect::new(x, y, w, h / 2.0)),
        PaneDropEdge::Bottom => (OverlayKind::Half, Rect::new(x, y + h / 2.0, w, h / 2.0)),
        PaneDropEdge::OuterLeft => (OverlayKind::Outer, Rect::new(x, y, strip_w, h)),
        PaneDropEdge::OuterRight => (
            OverlayKind::Outer,
            Rect::new(x + w - strip_w, y, strip_w, h),
        ),
        PaneDropEdge::OuterTop => (OverlayKind::Outer, Rect::new(x, y, w, strip_h)),
        PaneDropEdge::OuterBottom => (
            OverlayKind::Outer,
            Rect::new(x, y + h - strip_h, w, strip_h),
        ),
        PaneDropEdge::Replace => (
            OverlayKind::Centre,
            Rect::new(
                x + inset_w,
                y + inset_h,
                w - 2.0 * inset_w,
                h - 2.0 * inset_h,
            ),
        ),
    };
    DropOverlay { kind, rect }
}

/// What a dragged pane's ghost says: its header's label, or
/// [`EMPTY_PANE_LABEL`] for a pane that shows no session.
#[must_use]
pub fn ghost_label(title: Option<&str>) -> String {
    title.unwrap_or(EMPTY_PANE_LABEL).to_owned()
}

/// The pane of `grid`, laid out in `bounds` with dividers `thickness` wide,
/// under the pointer at `at`, and the edge of it the pointer points at; none
/// over a divider or outside the grid.
#[must_use]
pub fn pane_target(
    grid: &GridNode,
    bounds: Rect,
    thickness: f32,
    at: (f32, f32),
) -> Option<(String, PaneDropEdge)> {
    let rects = tabs::pane_rects(grid, bounds, thickness);
    let sides = tabs::outer_edges(grid, bounds, thickness);
    let ((pane_id, rect), (_, outer)) = rects.into_iter().zip(sides).find(|((_, rect), _)| {
        at.0 >= rect.x
            && at.0 < rect.x + rect.width
            && at.1 >= rect.y
            && at.1 < rect.y + rect.height
    })?;
    let x = (at.0 - rect.x) / rect.width;
    let y = (at.1 - rect.y) / rect.height;
    Some((pane_id, drop_edge(x, y, outer)))
}

/// Whether `(x, y)` — fractions of a pane — lies in the box that swaps panes.
fn in_centre_box(x: f32, y: f32) -> bool {
    x > CENTRE_LOW && x < CENTRE_HIGH && y > CENTRE_LOW && y < CENTRE_HIGH
}

/// The edge of a pane a pointer at `(x, y)` — its fractions of the pane's
/// width and height — points at, given that pane's `outer` sides.
///
/// A pointer in the outer band of a side the pane touches goes to the outer
/// edge, the nearest one at a corner; inside the centre box it replaces the
/// pane's session; elsewhere it takes the half of the axis it is further from
/// the middle of, and an exact diagonal tie takes the top or bottom half.
#[must_use]
pub fn drop_edge(x: f32, y: f32, outer: OuterSides) -> PaneDropEdge {
    let candidates = [
        (outer.left, x, PaneDropEdge::OuterLeft),
        (outer.right, 1.0 - x, PaneDropEdge::OuterRight),
        (outer.top, y, PaneDropEdge::OuterTop),
        (outer.bottom, 1.0 - y, PaneDropEdge::OuterBottom),
    ];
    let nearest = candidates
        .into_iter()
        .filter(|(on_edge, distance, _)| *on_edge && *distance < OUTER_BAND)
        .min_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((_, _, edge)) = nearest {
        return edge;
    }

    if in_centre_box(x, y) {
        return PaneDropEdge::Replace;
    }

    let dx = (x - 0.5).abs();
    let dy = (y - 0.5).abs();
    if dx > dy {
        return if x < 0.5 {
            PaneDropEdge::Left
        } else {
            PaneDropEdge::Right
        };
    }
    if y < 0.5 {
        PaneDropEdge::Top
    } else {
        PaneDropEdge::Bottom
    }
}

/// What a pane drag over a tab's pill does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillPaneHover {
    /// Show that tab, so the pane can drop on a pane in it.
    Activate,
    /// A diff tab takes no pane: the pill does not show that tab and shows
    /// the no-drop state.
    Inert,
    /// Nothing: the pill is the shown tab, or the strip has no such pill.
    Nothing,
}

/// Whether the pill of tab `pill` takes a pane dropped on it: a grid tab
/// does, a diff tab does not, and neither does a tab the strip lacks.
#[must_use]
pub fn pill_takes_pane(tabs: &[TabEntry], pill: &str) -> bool {
    tabs.iter()
        .find(|tab| tab.id == pill)
        .and_then(TabEntry::grid)
        .is_some()
}

/// What a pane drag over the pill of tab `pill` does, given the strip's
/// `tabs` and the shown tab `active`: a grid tab that is not shown is
/// activated, a diff tab is inert, and the shown tab and an unknown pill do
/// nothing.
#[must_use]
pub fn pill_pane_hover(tabs: &[TabEntry], active: Option<&str>, pill: &str) -> PillPaneHover {
    if !tabs.iter().any(|tab| tab.id == pill) {
        return PillPaneHover::Nothing;
    }
    if !pill_takes_pane(tabs, pill) {
        return PillPaneHover::Inert;
    }
    if active == Some(pill) {
        return PillPaneHover::Nothing;
    }
    PillPaneHover::Activate
}

/// Whether a pane drag still has somewhere to go: the pane is still in its
/// source tab, still shows the session it showed when the drag started (an
/// empty pane keeps dragging while it is there), and that session is still
/// in the client's model, as `session_exists` says.
#[must_use]
pub fn pane_drag_alive(
    tabs: &[TabEntry],
    src_tab: &str,
    pane_id: &str,
    session: Option<&str>,
    session_exists: impl FnOnce(&str) -> bool,
) -> bool {
    let Some(grid) = tabs
        .iter()
        .find(|tab| tab.id == src_tab)
        .and_then(TabEntry::grid)
    else {
        return false;
    };
    let Some(pane) = tabs::collect_panes(grid)
        .into_iter()
        .find(|pane| pane.id == pane_id)
    else {
        return false;
    };
    pane.session == session && session.is_none_or(session_exists)
}

/// Which half of a row or pill starting at `start` and `len` long `pos` points
/// at. A pointer exactly at the middle counts as the second half.
#[must_use]
pub fn side_of(pos: f32, start: f32, len: f32) -> Side {
    if pos < start + len / 2.0 {
        Side::Before
    } else {
        Side::After
    }
}

/// `ids` with `dragged` moved to `side` of `target`: before it or after it.
/// The same item, or one either side does not name, leaves the list as it is.
#[must_use]
pub fn reorder<T: Clone + PartialEq>(ids: &[T], dragged: &T, target: &T, side: Side) -> Vec<T> {
    let mut out = ids.to_vec();
    if dragged == target {
        return out;
    }
    let (Some(from), Some(target_at)) = (
        out.iter().position(|id| id == dragged),
        out.iter().position(|id| id == target),
    ) else {
        return out;
    };
    let moved = out.remove(from);
    let at = if from < target_at {
        target_at - 1
    } else {
        target_at
    };
    let at = match side {
        Side::Before => at,
        Side::After => at + 1,
    };
    out.insert(at, moved);
    out
}

/// The [`ClientMessage::ReorderTabs`] a pill dropped `side` of `target` sends,
/// or none when the order it would set is the one already in place.
/// `request_id` is the id the drop's reply answers, or none for the preview
/// a drag over a pill only asks whether the drop would change the order.
#[must_use]
pub fn reorder_tabs(
    tab_ids: &[String],
    dragged: &str,
    target: &str,
    side: Side,
    request_id: Option<String>,
) -> Option<ClientMessage> {
    let ordered = reorder(tab_ids, &dragged.to_owned(), &target.to_owned(), side);
    (ordered != tab_ids).then_some(ClientMessage::ReorderTabs {
        ordered_ids: ordered,
        request_id,
    })
}

/// The [`ClientMessage::ReorderContainers`] a container dropped `side` of
/// `target` sends, or none when the order it would set is the one already in
/// place.
#[must_use]
pub fn reorder_containers(
    containers: &[ContainerRef],
    dragged: &ContainerRef,
    target: &ContainerRef,
    side: Side,
) -> Option<ClientMessage> {
    let ordered = reorder(containers, dragged, target, side);
    (ordered != containers).then_some(ClientMessage::ReorderContainers { ordered })
}

/// The [`ClientMessage::ReorderSessions`] a leaf of `container_id` dropped
/// `side` of the leaf `target` sends: the container's whole shown order
/// `session_ids` with the move applied, or none when that order is the one
/// already in place.
#[must_use]
pub fn reorder_sessions(
    session_ids: &[String],
    container_id: &str,
    dragged: &str,
    target: &str,
    side: Side,
) -> Option<ClientMessage> {
    let ordered = reorder(session_ids, &dragged.to_owned(), &target.to_owned(), side);
    (ordered != session_ids).then(|| ClientMessage::ReorderSessions {
        container_id: container_id.to_owned(),
        ordered_ids: ordered,
    })
}

/// A sidebar row a drag can point at: a container's header row, by the
/// container's key, or a leaf, by its container's key and its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    Container(String),
    Leaf {
        container_key: String,
        session_id: String,
    },
}

/// The manual-order entry of a Repos-view `container`: a workspace's or a
/// repo's; none for any other kind.
#[must_use]
pub fn container_ref(container: &Container) -> Option<ContainerRef> {
    match container.kind {
        ContainerKind::Workspace => Some(ContainerRef::Workspace(container.id.clone())),
        ContainerKind::Repo => Some(ContainerRef::Repo(container.id.clone())),
        ContainerKind::Shell
        | ContainerKind::Dir
        | ContainerKind::Detached
        | ContainerKind::Tab
        | ContainerKind::Unbound => None,
    }
}

/// What a drag started on `container`'s header carries: a workspace or a
/// repo container itself, a tab container its tab as the strip's pill does;
/// SH, DIR, Detached and Unbound containers do not drag.
#[must_use]
pub fn container_payload(container: &Container) -> Option<DragPayload> {
    if container.kind == ContainerKind::Tab {
        return Some(DragPayload::Tab(container.id.clone()));
    }
    container_ref(container).map(DragPayload::Container)
}

/// What a drag started on the leaf of `session_id` in `container` carries:
/// every leaf drags, to a pane or a pill; only some reorder, as
/// [`leaf_reorders`] says.
#[must_use]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the sidebar's row builder takes an optional payload, as container rows may not drag"
)]
pub fn leaf_payload(container: &Container, session_id: &str) -> Option<DragPayload> {
    Some(DragPayload::Leaf {
        session_id: session_id.to_owned(),
        container_id: container.id.clone(),
    })
}

/// Whether the leaves of a `kind` container reorder by drag: a workspace's,
/// a repo's and a tab's do; SH, DIR, Detached and Unbound leaves keep their
/// order.
#[must_use]
pub fn leaf_reorders(kind: ContainerKind) -> bool {
    matches!(
        kind,
        ContainerKind::Workspace | ContainerKind::Repo | ContainerKind::Tab
    )
}

/// What a sidebar drop of `payload` at `side` of `row` sends, given the
/// sidebar's shown `containers`: a workspace or repo container dropped on
/// another one's header sends every shown workspace and repo in the new
/// order; a tab dropped on a tab container's header sends the new tab order;
/// a leaf dropped on another leaf of its own container sends that
/// container's new session order. Anything else, or a drop that keeps the
/// order, sends nothing.
#[must_use]
pub fn sidebar_reorder(
    payload: &DragPayload,
    containers: &[Container],
    row: &SidebarRow,
    side: Side,
) -> Option<ClientMessage> {
    match (payload, row) {
        (DragPayload::Container(dragged), SidebarRow::Container(key)) => {
            let target = containers
                .iter()
                .find(|c| &c.key == key)
                .and_then(container_ref)?;
            let shown: Vec<ContainerRef> = containers.iter().filter_map(container_ref).collect();
            reorder_containers(&shown, dragged, &target, side)
        }
        (DragPayload::Tab(dragged), SidebarRow::Container(key)) => {
            let tab_ids: Vec<String> = containers
                .iter()
                .filter(|c| c.kind == ContainerKind::Tab)
                .map(|c| c.id.clone())
                .collect();
            let target = containers
                .iter()
                .find(|c| &c.key == key && c.kind == ContainerKind::Tab)?;
            reorder_tabs(&tab_ids, dragged, &target.id, side, None)
        }
        (
            DragPayload::Leaf {
                session_id,
                container_id,
            },
            SidebarRow::Leaf {
                container_key,
                session_id: target,
            },
        ) => {
            let container = containers.iter().find(|c| &c.key == container_key)?;
            if &container.id != container_id || !leaf_reorders(container.kind) {
                return None;
            }
            let ids: Vec<String> = container.leaves.iter().map(|l| l.id.clone()).collect();
            reorder_sessions(&ids, container_id, session_id, target, side)
        }
        _ => None,
    }
}

/// Whether `pane_id` is a pane of `grid`.
fn contains_pane(grid: &GridNode, pane_id: &str) -> bool {
    match grid {
        GridNode::Pane { pane_id: id, .. } => id == pane_id,
        GridNode::Split { first, second, .. } => {
            contains_pane(first, pane_id) || contains_pane(second, pane_id)
        }
    }
}

/// Whether the pane `pane_id` of `grid` shows a session.
fn pane_is_bound(grid: &GridNode, pane_id: &str) -> bool {
    match grid {
        GridNode::Pane {
            pane_id: id,
            session_id,
        } => id == pane_id && session_id.is_some(),
        GridNode::Split { first, second, .. } => {
            pane_is_bound(first, pane_id) || pane_is_bound(second, pane_id)
        }
    }
}

/// Whether `node` is the pane `pane_id`.
fn is_pane(node: &GridNode, pane_id: &str) -> bool {
    matches!(node, GridNode::Pane { pane_id: id, .. } if id == pane_id)
}

/// Whether `src` and `dst` are the two panes of one split and `edge` is the
/// half of `dst` that faces `src`, at any depth: a move would rebuild the tree
/// it left.
fn drops_back_in_place(grid: &GridNode, src: &str, dst: &str, edge: PaneDropEdge) -> bool {
    let GridNode::Split {
        direction,
        first,
        second,
        ..
    } = grid
    else {
        return false;
    };
    let (first_face, second_face) = match direction {
        SplitDirection::Horizontal => (PaneDropEdge::Right, PaneDropEdge::Left),
        SplitDirection::Vertical => (PaneDropEdge::Bottom, PaneDropEdge::Top),
    };
    let facing = if is_pane(first, src) && is_pane(second, dst) {
        Some(second_face)
    } else if is_pane(second, src) && is_pane(first, dst) {
        Some(first_face)
    } else {
        None
    };
    if facing == Some(edge) {
        return true;
    }
    drops_back_in_place(first, src, dst, edge) || drops_back_in_place(second, src, dst, edge)
}

/// The message `payload` dropped on `dst_pane` of `dst_tab` sends, with the
/// undo label it records: none for a payload that is not a pane, a tab that is
/// gone or has no grid (a diff tab), a pane either grid does not hold, a pane
/// dropped on itself, and a drop that would rebuild the tree it left — the
/// daemon would clear the tab's picked layout for a move that changes nothing.
/// An edge or centre drop onto a pane counts the destination tab as arranged
/// by hand, and a centre drop in another tab counts both tabs; a centre drop
/// in another tab swaps the two panes' sessions when the destination pane
/// shows one, and moves the pane into the empty one when it does not.
#[must_use]
pub fn pane_drop(
    tabs: &[TabEntry],
    payload: &DragPayload,
    dst_tab: &str,
    dst_pane: &str,
    edge: PaneDropEdge,
    request_id: Option<String>,
) -> Option<PaneDrop> {
    let DragPayload::Pane {
        tab_id: src_tab_id,
        pane_id: src_pane_id,
    } = payload
    else {
        return None;
    };
    let src = tabs.iter().find(|tab| &tab.id == src_tab_id)?;
    let dst = tabs.iter().find(|tab| tab.id == dst_tab)?;
    let src_grid = src.grid()?;
    let dst_grid = dst.grid()?;
    if !contains_pane(src_grid, src_pane_id) || !contains_pane(dst_grid, dst_pane) {
        return None;
    }
    let within_one_tab = src_tab_id == dst_tab;
    if within_one_tab
        && (src_pane_id == dst_pane || drops_back_in_place(src_grid, src_pane_id, dst_pane, edge))
    {
        return None;
    }
    let bound = pane_is_bound(dst_grid, dst_pane);
    let swap = !within_one_tab && edge == PaneDropEdge::Replace && bound;
    let undo_label = if edge == PaneDropEdge::Replace && bound {
        undo::SWAPPED_PANES
    } else {
        undo::MOVED_PANE
    };
    Some(PaneDrop {
        src_tab_id: src_tab_id.clone(),
        src_pane_id: src_pane_id.clone(),
        dst_tab_id: dst_tab.to_owned(),
        dst_pane_id: dst_pane.to_owned(),
        edge,
        touch: true,
        swap,
        request_id,
        undo_label,
    })
}

/// What a sidebar leaf dropped on a pane or a pill does.
#[derive(Debug, Clone)]
pub enum LeafDrop {
    /// A bound leaf: one of its session's panes moves, as a pane drag
    /// would, with that move's undo entry.
    Move(PaneDrop),
    /// An unbound leaf: this message places its session, with no undo, as
    /// a click on the leaf places it.
    Send(Box<ClientMessage>),
}

/// The pane of a tab a leaf is dropped on, and the edge of it the pointer
/// points at.
#[derive(Debug, Clone, Copy)]
pub struct LeafTarget<'a> {
    pub tab_id: &'a str,
    pub pane_id: &'a str,
    pub edge: PaneDropEdge,
}

/// `session_id` among `sessions` when a pane can show it: not headless.
fn attachable<'a>(
    sessions: &'a [SessionSnapshot],
    session_id: &str,
) -> Option<&'a SessionSnapshot> {
    sessions
        .iter()
        .find(|s| s.id == session_id)
        .filter(|s| can_attach(s))
}

/// The pane a bound leaf of `session_id` moves, as tab and pane ids: the
/// active tab's pane showing it, else the first pane showing it in tab
/// order. Tabs that are closing do not count; none when no other pane shows
/// it, so the leaf drops as unbound.
#[must_use]
pub fn leaf_source_pane(tabs: &TabsModel, session_id: &str) -> Option<(String, String)> {
    let open = |tab: &&TabEntry| !tabs.closing().contains(&tab.id);
    let showing = |tab: &TabEntry| {
        tabs::collect_panes(tab.grid()?)
            .into_iter()
            .find(|pane| pane.session == Some(session_id))
            .map(|pane| (tab.id.clone(), pane.id.to_owned()))
    };
    tabs.active_tab()
        .filter(open)
        .and_then(showing)
        .or_else(|| tabs.tabs().iter().filter(open).find_map(showing))
}

/// The half of a pane an outer edge stands for, for a leaf: an unbound
/// session splits the pane it is over, never the whole grid.
fn half_of(edge: PaneDropEdge) -> PaneDropEdge {
    match edge {
        PaneDropEdge::OuterLeft => PaneDropEdge::Left,
        PaneDropEdge::OuterRight => PaneDropEdge::Right,
        PaneDropEdge::OuterTop => PaneDropEdge::Top,
        PaneDropEdge::OuterBottom => PaneDropEdge::Bottom,
        PaneDropEdge::Left
        | PaneDropEdge::Right
        | PaneDropEdge::Top
        | PaneDropEdge::Bottom
        | PaneDropEdge::Replace => edge,
    }
}

/// The split a drop on half `edge` of a pane makes; none for the centre or
/// an outer edge.
fn split_of(edge: PaneDropEdge) -> Option<(SplitDirection, SplitPlace)> {
    match edge {
        PaneDropEdge::Left => Some((SplitDirection::Horizontal, SplitPlace::First)),
        PaneDropEdge::Right => Some((SplitDirection::Horizontal, SplitPlace::Second)),
        PaneDropEdge::Top => Some((SplitDirection::Vertical, SplitPlace::First)),
        PaneDropEdge::Bottom => Some((SplitDirection::Vertical, SplitPlace::Second)),
        PaneDropEdge::Replace
        | PaneDropEdge::OuterLeft
        | PaneDropEdge::OuterRight
        | PaneDropEdge::OuterTop
        | PaneDropEdge::OuterBottom => None,
    }
}

/// The half edge of a pane a split by `direction` puts its new pane on at
/// `place`.
fn edge_of_split(direction: SplitDirection, place: SplitPlace) -> PaneDropEdge {
    match (direction, place) {
        (SplitDirection::Horizontal, SplitPlace::First) => PaneDropEdge::Left,
        (SplitDirection::Horizontal, SplitPlace::Second) => PaneDropEdge::Right,
        (SplitDirection::Vertical, SplitPlace::First) => PaneDropEdge::Top,
        (SplitDirection::Vertical, SplitPlace::Second) => PaneDropEdge::Bottom,
    }
}

/// Where the unbound `session_id` dropped at `target` goes: an empty pane
/// or the centre takes it in place of what the pane shows, a half (or the
/// outer edge on that side) splits the pane with it.
fn unbound_drop(
    tabs: &[TabEntry],
    session_id: &str,
    target: &LeafTarget<'_>,
    request_id: Option<String>,
) -> Option<(PaneDropEdge, LeafDrop)> {
    let grid = tabs.iter().find(|tab| tab.id == target.tab_id)?.grid()?;
    if !contains_pane(grid, target.pane_id) {
        return None;
    }
    let tab_id = target.tab_id.to_owned();
    let pane_id = target.pane_id.to_owned();
    let session = Some(session_id.to_owned());
    let edge = half_of(target.edge);
    let split = split_of(edge).filter(|_| pane_is_bound(grid, target.pane_id));
    let Some((direction, place)) = split else {
        let message = ClientMessage::ReplacePaneSession {
            tab_id,
            pane_id,
            session_id: session,
        };
        return Some((PaneDropEdge::Replace, LeafDrop::Send(Box::new(message))));
    };
    let message = ClientMessage::SplitPane {
        tab_id,
        pane_id,
        direction,
        place,
        new_session_id: session,
        request_id,
    };
    Some((edge, LeafDrop::Send(Box::new(message))))
}

/// What the leaf of `session_id` dropped at `target` does, and the edge it
/// lands on, which the overlay shows: a bound leaf moves its pane (see
/// [`leaf_source_pane`]) as [`pane_drop`] would move it; an unbound one is
/// placed there (see `unbound_drop`). None for a session a pane cannot show
/// (headless, unknown) and for a drop [`pane_drop`] refuses.
#[must_use]
pub fn leaf_drop(
    tabs: &TabsModel,
    sessions: &[SessionSnapshot],
    session_id: &str,
    target: &LeafTarget<'_>,
    request_id: Option<String>,
) -> Option<(PaneDropEdge, LeafDrop)> {
    attachable(sessions, session_id)?;
    let Some((tab_id, pane_id)) = leaf_source_pane(tabs, session_id) else {
        return unbound_drop(tabs.tabs(), session_id, target, request_id);
    };
    let payload = DragPayload::Pane { tab_id, pane_id };
    let drop = pane_drop(
        tabs.tabs(),
        &payload,
        target.tab_id,
        target.pane_id,
        target.edge,
        request_id,
    )?;
    Some((drop.edge, LeafDrop::Move(drop)))
}

/// Whether the pill of tab `pill` takes a leaf: a grid tab that is not
/// closing does; a diff tab, a closing tab and a tab the strip lacks do not.
#[must_use]
pub fn pill_takes_leaf(tabs: &TabsModel, pill: &str) -> bool {
    !tabs.closing().contains(pill) && pill_takes_pane(tabs.tabs(), pill)
}

/// What the leaf of `session_id` dropped on the pill of tab `pill` does:
/// an unbound leaf is placed in that tab where [`TabsModel::place_in`]
/// says; a bound leaf's pane (see [`leaf_source_pane`]) moves to the spot
/// [`tabs::pane_target_for_session`] gives in that tab, and nothing happens
/// when the tab already shows the session. None for a pill that takes no
/// leaf ([`pill_takes_leaf`]) and a session a pane cannot show.
///
/// This differs on purpose from a pane dropped on a pill, which lands at
/// [`tabs::pick_balanced_drop_target`]: a leaf goes where placing its
/// session would put it, beside its repo's or workspace's panes.
#[must_use]
pub fn leaf_pill_drop(
    tabs: &TabsModel,
    sessions: &[SessionSnapshot],
    session_id: &str,
    pill: &str,
    request_id: Option<String>,
) -> Option<LeafDrop> {
    let session = attachable(sessions, session_id)?;
    if !pill_takes_leaf(tabs, pill) {
        return None;
    }
    let Some((src_tab, src_pane)) = leaf_source_pane(tabs, session_id) else {
        return match tabs.place_in(pill, session, sessions) {
            Placement::NewTab => None,
            placement @ Placement::Pane { .. } => Some(LeafDrop::Send(Box::new(
                placement.message(session_id, request_id),
            ))),
        };
    };
    let grid = tabs.tab(pill)?.grid()?;
    if tabs::collect_panes(grid)
        .iter()
        .any(|pane| pane.session == Some(session_id))
    {
        return None;
    }
    let (dst_pane, edge) = match tabs::pane_target_for_session(grid, sessions, session)? {
        PaneTarget::Replace { pane_id } => (pane_id, PaneDropEdge::Replace),
        PaneTarget::Split(split) => (split.pane_id, edge_of_split(split.direction, split.place)),
    };
    let payload = DragPayload::Pane {
        tab_id: src_tab,
        pane_id: src_pane,
    };
    let mut drop = pane_drop(tabs.tabs(), &payload, pill, &dst_pane, edge, request_id)?;
    drop.touch = false;
    Some(LeafDrop::Move(drop))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::tabs::tests::{diff_tab, pane, split, tab, wire};
    use protocol::SplitDirection;
    use serde_json::json;

    const H: SplitDirection = SplitDirection::Horizontal;
    const V: SplitDirection = SplitDirection::Vertical;

    /// `t1` is `a | b`, `t2` holds the lone pane `c`, `t3` is a diff tab.
    fn tabs() -> Vec<TabEntry> {
        vec![
            tab(
                "t1",
                &split(H, 0.5, pane("a", Some("s1")), pane("b", Some("s2"))),
            ),
            tab("t2", &pane("c", Some("s3"))),
            diff_tab("t3"),
        ]
    }

    /// One tab holding the two panes `a` and `b`, split `direction`.
    fn pair(direction: SplitDirection) -> Vec<TabEntry> {
        vec![tab(
            "p",
            &split(direction, 0.5, pane("a", Some("s1")), pane("b", Some("s2"))),
        )]
    }

    fn drag(tab_id: &str, pane_id: &str) -> DragPayload {
        DragPayload::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        }
    }

    fn dropped(
        tabs: &[TabEntry],
        payload: &DragPayload,
        dst_tab: &str,
        dst_pane: &str,
        edge: PaneDropEdge,
    ) -> Option<PaneDrop> {
        pane_drop(tabs, payload, dst_tab, dst_pane, edge, None)
    }

    #[test]
    fn centre_box_is_replace() {
        let inner = OuterSides::default();

        assert_eq!(drop_edge(0.5, 0.5, inner), PaneDropEdge::Replace);
        assert_eq!(
            drop_edge(0.36, 0.64, inner),
            PaneDropEdge::Replace,
            "the centre box spans 35% to 65% on both axes"
        );
        assert_eq!(drop_edge(0.351, 0.5, inner), PaneDropEdge::Replace);
        assert_eq!(
            drop_edge(0.649, 0.5, inner),
            PaneDropEdge::Replace,
            "just inside the box swaps"
        );
        assert_eq!(drop_edge(0.5, 0.351, inner), PaneDropEdge::Replace);
        assert_eq!(drop_edge(0.5, 0.649, inner), PaneDropEdge::Replace);
        assert_eq!(
            drop_edge(0.5, 0.35, inner),
            PaneDropEdge::Top,
            "exactly 35% down is outside the box"
        );
        assert_eq!(
            drop_edge(0.5, 0.65, inner),
            PaneDropEdge::Bottom,
            "exactly 65% down is outside the box"
        );
        assert_eq!(
            drop_edge(0.35, 0.5, inner),
            PaneDropEdge::Left,
            "exactly 35% is outside the box"
        );
        assert_eq!(
            drop_edge(0.65, 0.5, inner),
            PaneDropEdge::Right,
            "exactly 65% is outside the box"
        );
    }

    #[test]
    fn nearer_axis_picks_the_half() {
        let inner = OuterSides::default();

        assert_eq!(drop_edge(0.1, 0.5, inner), PaneDropEdge::Left);
        assert_eq!(drop_edge(0.9, 0.5, inner), PaneDropEdge::Right);
        assert_eq!(drop_edge(0.5, 0.1, inner), PaneDropEdge::Top);
        assert_eq!(drop_edge(0.5, 0.9, inner), PaneDropEdge::Bottom);
        assert_eq!(
            drop_edge(0.2, 0.3, inner),
            PaneDropEdge::Left,
            "the axis the pointer is further from the middle of wins"
        );
        assert_eq!(drop_edge(0.3, 0.2, inner), PaneDropEdge::Top);
        assert_eq!(
            drop_edge(0.2, 0.2, inner),
            PaneDropEdge::Top,
            "an exact diagonal tie goes to the top or bottom half"
        );
    }

    #[test]
    fn outer_band_only_where_the_pane_touches_the_grid_edge() {
        let left = OuterSides {
            left: true,
            ..OuterSides::default()
        };
        let right = OuterSides {
            right: true,
            ..OuterSides::default()
        };
        let top = OuterSides {
            top: true,
            ..OuterSides::default()
        };
        let bottom = OuterSides {
            bottom: true,
            ..OuterSides::default()
        };

        assert_eq!(drop_edge(0.05, 0.5, left), PaneDropEdge::OuterLeft);
        assert_eq!(drop_edge(0.119, 0.5, left), PaneDropEdge::OuterLeft);
        assert_eq!(
            drop_edge(0.12, 0.5, left),
            PaneDropEdge::Left,
            "exactly 12% is outside the band"
        );
        assert_eq!(drop_edge(0.95, 0.5, right), PaneDropEdge::OuterRight);
        assert_eq!(drop_edge(0.5, 0.05, top), PaneDropEdge::OuterTop);
        assert_eq!(drop_edge(0.5, 0.95, bottom), PaneDropEdge::OuterBottom);
        assert_eq!(drop_edge(0.881, 0.5, right), PaneDropEdge::OuterRight);
        assert_eq!(
            drop_edge(0.88, 0.5, right),
            PaneDropEdge::Right,
            "exactly 12% in from the right side is outside the band"
        );
        assert_eq!(drop_edge(0.5, 0.881, bottom), PaneDropEdge::OuterBottom);
        assert_eq!(
            drop_edge(0.5, 0.88, bottom),
            PaneDropEdge::Bottom,
            "exactly 12% up from the bottom side is outside the band"
        );
        assert_eq!(
            drop_edge(0.05, 0.5, OuterSides::default()),
            PaneDropEdge::Left,
            "an inner side has no outer band"
        );
        assert_eq!(
            drop_edge(0.05, 0.5, right),
            PaneDropEdge::Left,
            "a side the pane does not touch never takes the outer edge"
        );
    }

    #[test]
    fn grid_corner_takes_the_nearer_outer_edge() {
        let corner = OuterSides {
            left: true,
            right: true,
            top: true,
            bottom: true,
        };

        assert_eq!(drop_edge(0.03, 0.08, corner), PaneDropEdge::OuterLeft);
        assert_eq!(drop_edge(0.08, 0.03, corner), PaneDropEdge::OuterTop);
        assert_eq!(drop_edge(0.97, 0.95, corner), PaneDropEdge::OuterRight);
        assert_eq!(drop_edge(0.95, 0.97, corner), PaneDropEdge::OuterBottom);
        assert_eq!(drop_edge(0.97, 0.05, corner), PaneDropEdge::OuterRight);
        assert_eq!(drop_edge(0.05, 0.97, corner), PaneDropEdge::OuterBottom);
        assert_eq!(
            drop_edge(0.05, 0.05, corner),
            PaneDropEdge::OuterLeft,
            "equal distances resolve to the same side every time"
        );
    }

    #[test]
    fn self_drop_sends_nothing() {
        let tabs = tabs();
        let every_edge = [
            PaneDropEdge::Left,
            PaneDropEdge::Right,
            PaneDropEdge::Top,
            PaneDropEdge::Bottom,
            PaneDropEdge::Replace,
            PaneDropEdge::OuterLeft,
            PaneDropEdge::OuterRight,
            PaneDropEdge::OuterTop,
            PaneDropEdge::OuterBottom,
        ];

        for edge in every_edge {
            assert!(
                dropped(&tabs, &drag("t1", "a"), "t1", "a", edge).is_none(),
                "a pane dropped on itself sends nothing ({edge:?})"
            );
        }
    }

    #[test]
    fn a_drop_that_rebuilds_the_same_split_sends_nothing() {
        let side_by_side = pair(H);
        assert!(
            dropped(&side_by_side, &drag("p", "a"), "p", "b", PaneDropEdge::Left).is_none(),
            "a is already left of b"
        );
        assert!(
            dropped(
                &side_by_side,
                &drag("p", "b"),
                "p",
                "a",
                PaneDropEdge::Right
            )
            .is_none(),
            "b is already right of a"
        );
        assert!(
            dropped(
                &side_by_side,
                &drag("p", "a"),
                "p",
                "b",
                PaneDropEdge::Right
            )
            .is_some(),
            "the far half of b still moves a"
        );

        let stacked = pair(V);
        assert!(
            dropped(&stacked, &drag("p", "a"), "p", "b", PaneDropEdge::Top).is_none(),
            "a is already above b"
        );
        assert!(
            dropped(&stacked, &drag("p", "b"), "p", "a", PaneDropEdge::Bottom).is_none(),
            "b is already below a"
        );

        let nested = vec![tab(
            "n",
            &split(
                H,
                0.5,
                pane("a", Some("s1")),
                split(V, 0.5, pane("b", Some("s2")), pane("c", Some("s3"))),
            ),
        )];
        assert!(
            dropped(&nested, &drag("n", "b"), "n", "c", PaneDropEdge::Top).is_none(),
            "siblings under a nested split rebuild the tree the same way"
        );
    }

    #[test]
    fn pane_drop_refuses_a_pane_the_grid_does_not_hold() {
        let tabs = tabs();

        assert!(
            dropped(&tabs, &drag("t1", "gone"), "t2", "c", PaneDropEdge::Left).is_none(),
            "the source pane is not in its tab"
        );
        assert!(
            dropped(&tabs, &drag("t1", "a"), "t2", "gone", PaneDropEdge::Left).is_none(),
            "the destination pane is not in its tab"
        );
        assert!(
            dropped(
                &tabs,
                &drag("t1", "a"),
                "t2",
                "gone",
                PaneDropEdge::OuterLeft
            )
            .is_none(),
            "an outer edge ignores the destination pane id but still needs one the tab holds"
        );
    }

    #[test]
    fn only_pane_cannot_move_within_its_tab() {
        let tabs = tabs();

        assert!(
            dropped(
                &tabs,
                &drag("t2", "c"),
                "t2",
                "elsewhere",
                PaneDropEdge::Left
            )
            .is_none(),
            "a tab's only pane stays in that tab"
        );
        assert!(
            dropped(&tabs, &drag("t2", "c"), "t1", "b", PaneDropEdge::Left).is_some(),
            "the same pane moves to another tab"
        );
    }

    #[test]
    fn swap_is_labelled_swapped_panes() {
        let tabs = tabs();

        let swapped = dropped(&tabs, &drag("t1", "a"), "t1", "b", PaneDropEdge::Replace)
            .expect("a same-tab centre drop");
        assert_eq!(swapped.undo_label, undo::SWAPPED_PANES);
        assert!(
            matches!(
                &swapped.message(),
                ClientMessage::MovePane {
                    edge: PaneDropEdge::Replace,
                    ..
                }
            ),
            "{:?}",
            swapped.message()
        );

        let moved = dropped(&tabs, &drag("t1", "a"), "t1", "b", PaneDropEdge::Right)
            .expect("a same-tab edge drop");
        assert_eq!(moved.undo_label, undo::MOVED_PANE);
    }

    #[test]
    fn cross_tab_centre_drop_onto_a_bound_pane_swaps() {
        let tabs = tabs();

        let drop = pane_drop(
            &tabs,
            &drag("t1", "a"),
            "t2",
            "c",
            PaneDropEdge::Replace,
            Some("r7".to_owned()),
        )
        .expect("a cross-tab centre drop onto a bound pane swaps the two sessions");
        assert_eq!(drop.undo_label, undo::SWAPPED_PANES);
        assert_eq!(
            wire(&drop.message()),
            json!({
                "type": "swap_panes",
                "src_tab_id": "t1",
                "src_pane_id": "a",
                "dst_tab_id": "t2",
                "dst_pane_id": "c",
                "request_id": "r7",
            })
        );
        assert!(
            dropped(&tabs, &drag("t1", "a"), "t2", "c", PaneDropEdge::Right).is_some(),
            "an edge drop in another tab still moves the pane"
        );
    }

    #[test]
    fn cross_tab_centre_drop_onto_an_empty_pane_moves_with_touch() {
        let tabs = vec![
            tab(
                "t1",
                &split(H, 0.5, pane("a", Some("s1")), pane("b", Some("s2"))),
            ),
            tab("t2", &split(H, 0.5, pane("c", Some("s3")), pane("d", None))),
        ];

        let drop = dropped(&tabs, &drag("t1", "a"), "t2", "d", PaneDropEdge::Replace)
            .expect("a cross-tab centre drop onto an empty pane moves the pane");
        assert_eq!(drop.undo_label, undo::MOVED_PANE);
        assert!(
            matches!(
                &drop.message(),
                ClientMessage::MovePane {
                    edge: PaneDropEdge::Replace,
                    touch: true,
                    ..
                }
            ),
            "{:?}",
            drop.message()
        );
    }

    #[test]
    fn same_tab_centre_drop_onto_an_empty_pane_is_labelled_moved() {
        let tabs = vec![tab(
            "t1",
            &split(H, 0.5, pane("a", Some("s1")), pane("b", None)),
        )];

        let drop = dropped(&tabs, &drag("t1", "a"), "t1", "b", PaneDropEdge::Replace)
            .expect("a same-tab centre drop onto an empty pane");
        assert_eq!(
            drop.undo_label,
            undo::MOVED_PANE,
            "only a pane that shows a session swaps"
        );
    }

    #[test]
    fn edge_and_centre_drops_set_touch() {
        let tabs = tabs();

        for (dst_tab, dst_pane, edge) in [
            ("t1", "b", PaneDropEdge::Right),
            ("t1", "b", PaneDropEdge::Replace),
            ("t2", "c", PaneDropEdge::Right),
        ] {
            let drop =
                dropped(&tabs, &drag("t1", "a"), dst_tab, dst_pane, edge).expect("the drop lands");
            assert!(
                matches!(&drop.message(), ClientMessage::MovePane { touch: true, .. }),
                "a {edge:?} drop onto {dst_pane} touches: {:?}",
                drop.message()
            );
        }
    }

    #[test]
    fn pill_hover_activates_a_grid_tab() {
        let tabs = tabs();

        assert_eq!(
            pill_pane_hover(&tabs, Some("t1"), "t2"),
            PillPaneHover::Activate
        );
        assert_eq!(
            pill_pane_hover(&tabs, Some("t3"), "t1"),
            PillPaneHover::Activate,
            "a grid tab takes a pane from any other tab, a diff one included"
        );
    }

    #[test]
    fn pill_hover_is_inert_on_a_diff_tab() {
        let tabs = tabs();

        assert_eq!(
            pill_pane_hover(&tabs, Some("t1"), "t3"),
            PillPaneHover::Inert,
            "a diff tab takes no pane"
        );
    }

    #[test]
    fn pill_hover_on_the_active_tab_is_nothing() {
        let tabs = tabs();

        assert_eq!(
            pill_pane_hover(&tabs, Some("t1"), "t1"),
            PillPaneHover::Nothing
        );
        assert_eq!(
            pill_pane_hover(&tabs, None, "gone"),
            PillPaneHover::Nothing,
            "a pill the strip does not hold"
        );
    }

    /// The sessions the [`tabs`] fixture's panes show.
    fn known_session(id: &str) -> bool {
        ["s1", "s2", "s3"].contains(&id)
    }

    #[test]
    fn pane_drag_alive_while_the_pane_and_session_stand() {
        let tabs = tabs();

        assert!(pane_drag_alive(&tabs, "t1", "a", Some("s1"), known_session));
        assert!(pane_drag_alive(&tabs, "t1", "b", Some("s2"), known_session));
        assert!(
            pane_drag_alive(&tabs, "t2", "c", Some("s3"), known_session),
            "a tab's only pane drags"
        );
    }

    #[test]
    fn pane_drag_dies_with_its_pane() {
        let tabs = tabs();

        assert!(!pane_drag_alive(
            &tabs,
            "t1",
            "gone",
            Some("s1"),
            known_session
        ));
        assert!(
            !pane_drag_alive(&tabs, "gone", "a", Some("s1"), known_session),
            "the source tab left the strip"
        );
        assert!(
            !pane_drag_alive(&tabs, "t3", "a", Some("s1"), known_session),
            "a diff tab holds no pane"
        );
    }

    #[test]
    fn pane_drag_dies_with_its_session() {
        let tabs = tabs();

        assert!(
            !pane_drag_alive(&tabs, "t1", "a", Some("s1"), |_| false),
            "the session left the client, so the pane has nowhere to go back to"
        );
    }

    #[test]
    fn pane_drag_dies_when_the_pane_takes_another_session() {
        let tabs = tabs();

        assert!(!pane_drag_alive(
            &tabs,
            "t1",
            "a",
            Some("s2"),
            known_session
        ));
        assert!(
            !pane_drag_alive(&tabs, "t1", "a", None, known_session),
            "a pane that showed a session and now shows none"
        );
    }

    #[test]
    fn an_empty_pane_keeps_dragging_while_it_is_there() {
        let tabs = vec![tab("t1", &pane("e", None))];

        assert!(pane_drag_alive(&tabs, "t1", "e", None, known_session));
        assert!(
            !pane_drag_alive(&tabs, "t1", "e", Some("s1"), known_session),
            "an empty pane the drag started on does not become a session's"
        );
    }

    /// A pane 200 by 100 whose corner is not at the origin.
    const PANE: Rect = Rect::new(10.0, 20.0, 200.0, 100.0);

    fn assert_overlay(edge: PaneDropEdge, kind: OverlayKind, expected: Rect) {
        let overlay = drop_overlay(edge, PANE);
        assert_eq!(overlay.kind, kind, "{edge:?}");
        let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
        let got = overlay.rect;
        assert!(
            close(got.x, expected.x)
                && close(got.y, expected.y)
                && close(got.width, expected.width)
                && close(got.height, expected.height),
            "{edge:?}: {got:?} is not {expected:?}"
        );
    }

    #[test]
    fn a_half_overlay_covers_that_half_of_the_pane() {
        let half = OverlayKind::Half;
        assert_overlay(
            PaneDropEdge::Left,
            half,
            Rect::new(10.0, 20.0, 100.0, 100.0),
        );
        assert_overlay(
            PaneDropEdge::Right,
            half,
            Rect::new(110.0, 20.0, 100.0, 100.0),
        );
        assert_overlay(PaneDropEdge::Top, half, Rect::new(10.0, 20.0, 200.0, 50.0));
        assert_overlay(
            PaneDropEdge::Bottom,
            half,
            Rect::new(10.0, 70.0, 200.0, 50.0),
        );
    }

    #[test]
    fn an_outer_strip_runs_along_its_side_for_14_percent_of_the_pane() {
        let outer = OverlayKind::Outer;
        assert_overlay(
            PaneDropEdge::OuterLeft,
            outer,
            Rect::new(10.0, 20.0, 28.0, 100.0),
        );
        assert_overlay(
            PaneDropEdge::OuterRight,
            outer,
            Rect::new(182.0, 20.0, 28.0, 100.0),
        );
        assert_overlay(
            PaneDropEdge::OuterTop,
            outer,
            Rect::new(10.0, 20.0, 200.0, 14.0),
        );
        assert_overlay(
            PaneDropEdge::OuterBottom,
            outer,
            Rect::new(10.0, 106.0, 200.0, 14.0),
        );
    }

    #[test]
    fn the_centre_overlay_is_inset_12_percent_on_every_side() {
        assert_overlay(
            PaneDropEdge::Replace,
            OverlayKind::Centre,
            Rect::new(34.0, 32.0, 152.0, 76.0),
        );
    }

    #[test]
    fn the_ghost_shows_the_header_label() {
        assert_eq!(ghost_label(Some("api work")), "api work");
        assert_eq!(
            ghost_label(None),
            "Empty pane",
            "an empty pane's ghost says what its header says"
        );
    }

    #[test]
    fn reorder_before_and_after() {
        let ids = ["a", "b", "c", "d"];

        assert_eq!(reorder(&ids, &"a", &"c", Side::After), ["b", "c", "a", "d"]);
        assert_eq!(
            reorder(&ids, &"a", &"c", Side::Before),
            ["b", "a", "c", "d"]
        );
        assert_eq!(
            reorder(&ids, &"d", &"a", Side::Before),
            ["d", "a", "b", "c"]
        );
        assert_eq!(reorder(&ids, &"d", &"a", Side::After), ["a", "d", "b", "c"]);
        assert_eq!(
            reorder(&ids, &"a", &"d", Side::After),
            ["b", "c", "d", "a"],
            "the dragged item goes last"
        );
        assert_eq!(
            reorder(&ids, &"b", &"a", Side::After),
            ids,
            "already after its neighbour"
        );
        assert_eq!(
            reorder(&ids, &"b", &"c", Side::Before),
            ids,
            "already before its neighbour"
        );
    }

    #[test]
    fn reorder_onto_itself_is_unchanged() {
        let ids = ["a", "b", "c"];

        assert_eq!(reorder(&ids, &"b", &"b", Side::Before), ids);
        assert_eq!(reorder(&ids, &"b", &"b", Side::After), ids);
        assert_eq!(reorder(&ids, &"gone", &"b", Side::After), ids);
        assert_eq!(reorder(&ids, &"b", &"gone", Side::Before), ids);
        assert_eq!(reorder(&["a"], &"a", &"a", Side::After), ["a"]);
    }

    #[test]
    fn reorder_tabs_message_moves_first_to_last() {
        let ids: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| (*id).to_owned())
            .collect();

        let message = reorder_tabs(&ids, "a", "d", Side::After, None).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_tabs", "ordered_ids": ["b", "c", "d", "a"]}),
            "the first pill goes last"
        );

        let message = reorder_tabs(&ids, "d", "a", Side::Before, None).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_tabs", "ordered_ids": ["d", "a", "b", "c"]}),
            "the last pill goes first"
        );

        let message =
            reorder_tabs(&ids, "a", "b", Side::After, None).expect("a swap of neighbours");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_tabs", "ordered_ids": ["b", "a", "c", "d"]}),
            "an adjacent move still changes the order"
        );
    }

    #[test]
    fn reorder_tabs_onto_itself_sends_nothing() {
        let ids: Vec<String> = ["a", "b"].iter().map(|id| (*id).to_owned()).collect();

        assert!(reorder_tabs(&ids, "a", "a", Side::After, None).is_none());
        assert!(
            reorder_tabs(&ids, "a", "b", Side::Before, None).is_none(),
            "a drop that leaves the order as it is sends nothing"
        );
    }

    #[test]
    fn reorder_containers_message_keeps_the_full_order() {
        let containers = [
            ContainerRef::Repo("r1".to_owned()),
            ContainerRef::Workspace("w1".to_owned()),
            ContainerRef::Repo("r2".to_owned()),
        ];
        let r1 = containers.first().expect("the first container");
        let r2 = containers.get(2).expect("the third container");

        let message = reorder_containers(&containers, r2, r1, Side::Before).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({
                "type": "reorder_containers",
                "ordered": [
                    {"kind": "repo", "id": "r2"},
                    {"kind": "repo", "id": "r1"},
                    {"kind": "workspace", "id": "w1"},
                ],
            }),
            "every container keeps its place but the dragged one"
        );

        assert!(
            reorder_containers(&containers, r1, r1, Side::After).is_none(),
            "a container dropped on itself sends nothing"
        );
    }

    /// The Repos view of workspace `w1`, repos `r1` (sessions `a`, `b`, `c`)
    /// and `r2` (session `d`), the standalone shell `sh`, the folder shell
    /// `dir` and the detached session `x`.
    fn repos_view() -> Vec<Container> {
        use crate::sidebar::tests::{in_repo, repo, shell, workspace};
        use protocol::{DaemonMessage, SessionKind};
        let mut model = crate::sidebar::SidebarModel::default();
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")],
        });
        model.apply(&DaemonMessage::Workspaces {
            workspaces: vec![workspace("w1", &[])],
        });
        model.apply(&DaemonMessage::Sessions {
            sessions: vec![
                in_repo("a", "r1"),
                in_repo("b", "r1"),
                in_repo("c", "r1"),
                in_repo("d", "r2"),
                shell("sh", Some("C:\\elsewhere"), SessionKind::Standalone),
                shell("dir", Some("C:\\other\\proj"), SessionKind::Single),
                in_repo("x", "gone"),
            ],
        });
        model.containers()
    }

    /// The Tabs view of `t1` (sessions `a`, `b`), `t2` (session `c`) and the
    /// unbound session `u`.
    fn tabs_view() -> Vec<Container> {
        use crate::sidebar::tests::session;
        use protocol::DaemonMessage;
        let mut model = crate::sidebar::SidebarModel::default();
        model.apply(&DaemonMessage::Sessions {
            sessions: vec![session("a"), session("b"), session("c"), session("u")],
        });
        model.tab_containers(&[
            tab(
                "t1",
                &split(H, 0.5, pane("p1", Some("a")), pane("p2", Some("b"))),
            ),
            tab("t2", &pane("p3", Some("c"))),
        ])
    }

    fn of_kind(containers: &[Container], kind: ContainerKind) -> &Container {
        containers
            .iter()
            .find(|c| c.kind == kind)
            .expect("a container of that kind")
    }

    fn header(key: &str) -> SidebarRow {
        SidebarRow::Container(key.to_owned())
    }

    fn leaf_row(container_key: &str, session_id: &str) -> SidebarRow {
        SidebarRow::Leaf {
            container_key: container_key.to_owned(),
            session_id: session_id.to_owned(),
        }
    }

    fn leaf_drag(session_id: &str, container_id: &str) -> DragPayload {
        DragPayload::Leaf {
            session_id: session_id.to_owned(),
            container_id: container_id.to_owned(),
        }
    }

    #[test]
    fn only_workspace_repo_and_tab_containers_drag() {
        let repos = repos_view();
        let payload = |kind| container_payload(of_kind(&repos, kind));
        assert_eq!(
            payload(ContainerKind::Workspace),
            Some(DragPayload::Container(ContainerRef::Workspace(
                "w1".to_owned()
            )))
        );
        assert_eq!(
            payload(ContainerKind::Repo),
            Some(DragPayload::Container(ContainerRef::Repo("r1".to_owned())))
        );
        for kind in [
            ContainerKind::Shell,
            ContainerKind::Dir,
            ContainerKind::Detached,
        ] {
            assert_eq!(payload(kind), None, "{kind:?} does not drag");
        }

        let tabs = tabs_view();
        assert_eq!(
            container_payload(of_kind(&tabs, ContainerKind::Tab)),
            Some(DragPayload::Tab("t1".to_owned())),
            "a tab container drags the strip's own payload"
        );
        assert_eq!(
            container_payload(of_kind(&tabs, ContainerKind::Unbound)),
            None
        );
    }

    #[test]
    fn every_leaf_drags_and_only_these_reorder() {
        let repos = repos_view();
        let tabs = tabs_view();
        for kind in [
            ContainerKind::Workspace,
            ContainerKind::Repo,
            ContainerKind::Shell,
            ContainerKind::Dir,
            ContainerKind::Detached,
        ] {
            let container = of_kind(&repos, kind);
            let Some(leaf) = container.leaves.first() else {
                continue;
            };
            assert_eq!(
                leaf_payload(container, &leaf.id),
                Some(leaf_drag(&leaf.id, &container.id)),
                "{kind:?} leaves drag"
            );
        }
        assert_eq!(
            leaf_payload(of_kind(&repos, ContainerKind::Repo), "a"),
            Some(leaf_drag("a", "r1"))
        );
        assert_eq!(
            leaf_payload(of_kind(&tabs, ContainerKind::Tab), "b"),
            Some(leaf_drag("b", "t1"))
        );
        let unbound = of_kind(&tabs, ContainerKind::Unbound);
        assert_eq!(
            leaf_payload(unbound, "u"),
            Some(leaf_drag("u", &unbound.id))
        );

        for kind in [
            ContainerKind::Workspace,
            ContainerKind::Repo,
            ContainerKind::Tab,
        ] {
            assert!(leaf_reorders(kind), "{kind:?} leaves reorder");
        }
        for kind in [
            ContainerKind::Shell,
            ContainerKind::Dir,
            ContainerKind::Detached,
            ContainerKind::Unbound,
        ] {
            assert!(!leaf_reorders(kind), "{kind:?} leaves keep their order");
        }
    }

    /// `t1` is `a | b` showing `s1` and `s2`; `t2` is `c` showing `s1` over
    /// the empty `e`; `t3` is a diff tab; `t4` is the lone empty pane `f`.
    fn leaf_tabs() -> tabs::TabsModel {
        crate::tabs::tests::model_with(&[
            tab(
                "t1",
                &split(H, 0.5, pane("a", Some("s1")), pane("b", Some("s2"))),
            ),
            tab("t2", &split(V, 0.5, pane("c", Some("s1")), pane("e", None))),
            diff_tab("t3"),
            tab("t4", &pane("f", None)),
        ])
    }

    /// `s1`, `s2` and the unbound `u` attach; `h` is headless.
    fn leaf_sessions() -> Vec<protocol::SessionSnapshot> {
        use crate::sidebar::tests::session;
        let mut headless = session("h");
        headless.mode = protocol::SessionMode::Headless;
        vec![session("s1"), session("s2"), session("u"), headless]
    }

    fn onto<'a>(tab_id: &'a str, pane_id: &'a str, edge: PaneDropEdge) -> LeafTarget<'a> {
        LeafTarget {
            tab_id,
            pane_id,
            edge,
        }
    }

    /// The edge a leaf drop lands on, and what it sends on the wire.
    fn landed(drop: Option<(PaneDropEdge, LeafDrop)>) -> (PaneDropEdge, serde_json::Value) {
        let (edge, drop) = drop.expect("the leaf drops");
        let message = match drop {
            LeafDrop::Move(pane) => pane.message(),
            LeafDrop::Send(message) => *message,
        };
        (edge, wire(&message))
    }

    fn moved(drop: Option<LeafDrop>) -> PaneDrop {
        match drop {
            Some(LeafDrop::Move(pane)) => Some(pane),
            _ => None,
        }
        .expect("a pane move")
    }

    #[test]
    fn bound_leaf_moves_its_active_tab_pane() {
        let sessions = leaf_sessions();
        let mut model = leaf_tabs();
        model.activate("t2");
        let (edge, drop) = leaf_drop(
            &model,
            &sessions,
            "s1",
            &onto("t1", "b", PaneDropEdge::Right),
            Some("r".to_owned()),
        )
        .expect("the leaf drops");
        assert_eq!(edge, PaneDropEdge::Right);
        let drop = moved(Some(drop));
        assert_eq!(
            (drop.src_tab_id.as_str(), drop.src_pane_id.as_str()),
            ("t2", "c"),
            "the active tab's pane is the source"
        );
        assert_eq!(drop.undo_label, undo::MOVED_PANE);
        assert!(drop.touch);

        model.activate("t4");
        let (_, drop) = leaf_drop(
            &model,
            &sessions,
            "s1",
            &onto("t4", "f", PaneDropEdge::Replace),
            None,
        )
        .expect("the leaf drops");
        let drop = moved(Some(drop));
        assert_eq!(
            (drop.src_tab_id.as_str(), drop.src_pane_id.as_str()),
            ("t1", "a"),
            "with no binding in the active tab, the first in tab order"
        );

        model.activate("t1");
        assert!(
            leaf_drop(
                &model,
                &sessions,
                "s1",
                &onto("t1", "a", PaneDropEdge::Left),
                None
            )
            .is_none(),
            "a leaf dropped on its own pane does nothing"
        );
    }

    #[test]
    fn a_binding_in_a_closing_tab_counts_as_unbound() {
        let sessions = leaf_sessions();
        let mut model = leaf_tabs();
        model.mark_closing("t1");
        let drop = leaf_drop(
            &model,
            &sessions,
            "s2",
            &onto("t4", "f", PaneDropEdge::Replace),
            None,
        );
        assert_eq!(
            landed(drop),
            (
                PaneDropEdge::Replace,
                json!({
                    "type": "replace_pane_session",
                    "tab_id": "t4",
                    "pane_id": "f",
                    "session_id": "s2",
                })
            ),
            "s2's only pane is in a closing tab: it is placed, not moved"
        );

        let (_, message) = landed(
            leaf_pill_drop(&model, &sessions, "s2", "t2", None)
                .map(|drop| (PaneDropEdge::Replace, drop)),
        );
        assert_eq!(
            message,
            json!({
                "type": "replace_pane_session",
                "tab_id": "t2",
                "pane_id": "e",
                "session_id": "s2",
            }),
            "on a pill too"
        );
    }

    #[test]
    fn unbound_leaf_on_a_half_splits_with_the_session() {
        let sessions = leaf_sessions();
        let model = leaf_tabs();
        for (edge, direction, place) in [
            (PaneDropEdge::Left, "horizontal", "first"),
            (PaneDropEdge::Right, "horizontal", "second"),
            (PaneDropEdge::Top, "vertical", "first"),
            (PaneDropEdge::Bottom, "vertical", "second"),
        ] {
            let drop = leaf_drop(
                &model,
                &sessions,
                "u",
                &onto("t1", "b", edge),
                Some("r".to_owned()),
            );
            assert_eq!(
                landed(drop),
                (
                    edge,
                    json!({
                        "type": "split_pane",
                        "tab_id": "t1",
                        "pane_id": "b",
                        "direction": direction,
                        "place": place,
                        "new_session_id": "u",
                        "request_id": "r",
                    })
                ),
                "{edge:?}"
            );
        }
    }

    #[test]
    fn unbound_leaf_on_the_centre_replaces() {
        let sessions = leaf_sessions();
        let drop = leaf_drop(
            &leaf_tabs(),
            &sessions,
            "u",
            &onto("t1", "b", PaneDropEdge::Replace),
            Some("r".to_owned()),
        );
        assert_eq!(
            landed(drop),
            (
                PaneDropEdge::Replace,
                json!({
                    "type": "replace_pane_session",
                    "tab_id": "t1",
                    "pane_id": "b",
                    "session_id": "u",
                })
            )
        );
    }

    #[test]
    fn unbound_leaf_on_an_outer_edge_splits_that_side_of_the_pane() {
        let sessions = leaf_sessions();
        let model = leaf_tabs();
        for (pane_id, outer, half, direction, place) in [
            (
                "a",
                PaneDropEdge::OuterLeft,
                PaneDropEdge::Left,
                "horizontal",
                "first",
            ),
            (
                "b",
                PaneDropEdge::OuterRight,
                PaneDropEdge::Right,
                "horizontal",
                "second",
            ),
            (
                "a",
                PaneDropEdge::OuterTop,
                PaneDropEdge::Top,
                "vertical",
                "first",
            ),
            (
                "b",
                PaneDropEdge::OuterBottom,
                PaneDropEdge::Bottom,
                "vertical",
                "second",
            ),
        ] {
            let drop = leaf_drop(
                &model,
                &sessions,
                "u",
                &onto("t1", pane_id, outer),
                Some("r".to_owned()),
            );
            assert_eq!(
                landed(drop),
                (
                    half,
                    json!({
                        "type": "split_pane",
                        "tab_id": "t1",
                        "pane_id": pane_id,
                        "direction": direction,
                        "place": place,
                        "new_session_id": "u",
                        "request_id": "r",
                    })
                ),
                "{outer:?} lands on the pane's {half:?}"
            );
        }
    }

    #[test]
    fn unbound_leaf_on_an_empty_pane_fills_it() {
        let sessions = leaf_sessions();
        let model = leaf_tabs();
        for edge in [
            PaneDropEdge::Left,
            PaneDropEdge::Bottom,
            PaneDropEdge::OuterBottom,
            PaneDropEdge::Replace,
        ] {
            let drop = leaf_drop(&model, &sessions, "u", &onto("t2", "e", edge), None);
            assert_eq!(
                landed(drop),
                (
                    PaneDropEdge::Replace,
                    json!({
                        "type": "replace_pane_session",
                        "tab_id": "t2",
                        "pane_id": "e",
                        "session_id": "u",
                    })
                ),
                "{edge:?}"
            );
        }
    }

    #[test]
    fn a_headless_leaf_drops_nowhere() {
        let sessions = leaf_sessions();
        let model = leaf_tabs();
        for session in ["h", "gone"] {
            assert!(
                leaf_drop(
                    &model,
                    &sessions,
                    session,
                    &onto("t1", "b", PaneDropEdge::Left),
                    None
                )
                .is_none(),
                "{session} drops on no pane"
            );
            assert!(
                leaf_pill_drop(&model, &sessions, session, "t4", None).is_none(),
                "{session} drops on no pill"
            );
        }
        assert!(
            leaf_drop(
                &model,
                &sessions,
                "u",
                &onto("t1", "nowhere", PaneDropEdge::Left),
                None
            )
            .is_none(),
            "a pane the tab lacks takes no leaf"
        );
    }

    #[test]
    fn a_leaf_on_a_diff_tab_pill_is_refused() {
        let sessions = leaf_sessions();
        let mut model = leaf_tabs();
        for session in ["u", "s2"] {
            for pill in ["t3", "t9"] {
                assert!(
                    leaf_pill_drop(&model, &sessions, session, pill, None).is_none(),
                    "{session} on {pill}"
                );
            }
        }
        assert!(!pill_takes_leaf(&model, "t3"));
        assert!(pill_takes_leaf(&model, "t4"));

        let (_, message) = landed(
            leaf_pill_drop(&model, &sessions, "u", "t4", None)
                .map(|drop| (PaneDropEdge::Replace, drop)),
        );
        assert_eq!(
            message,
            json!({
                "type": "replace_pane_session",
                "tab_id": "t4",
                "pane_id": "f",
                "session_id": "u",
            })
        );

        model.mark_closing("t4");
        assert!(!pill_takes_leaf(&model, "t4"));
        for session in ["u", "s2"] {
            assert!(
                leaf_pill_drop(&model, &sessions, session, "t4", None).is_none(),
                "a closing tab takes no {session}"
            );
        }
    }

    #[test]
    fn a_bound_leaf_on_a_pill_of_a_tab_showing_it_does_nothing() {
        let sessions = leaf_sessions();
        let model = leaf_tabs();
        for pill in ["t1", "t2"] {
            assert!(
                leaf_pill_drop(&model, &sessions, "s1", pill, None).is_none(),
                "{pill} already shows s1"
            );
        }

        let drop = moved(leaf_pill_drop(
            &model,
            &sessions,
            "s2",
            "t2",
            Some("r".to_owned()),
        ));
        assert_eq!(
            wire(&drop.message()),
            json!({
                "type": "move_pane",
                "src_tab_id": "t1",
                "src_pane_id": "b",
                "dst_tab_id": "t2",
                "dst_pane_id": "e",
                "edge": "replace",
                "request_id": "r",
            }),
            "the session's pane goes where pane_target_for_session says"
        );
        assert!(!drop.touch, "a pill drop leaves the tab untouched");
        assert_eq!(drop.undo_label, undo::MOVED_PANE);
    }

    #[test]
    fn container_drop_sends_every_shown_workspace_and_repo() {
        let repos = repos_view();
        let r2 = DragPayload::Container(ContainerRef::Repo("r2".to_owned()));

        let message =
            sidebar_reorder(&r2, &repos, &header("ws:w1"), Side::Before).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({
                "type": "reorder_containers",
                "ordered": [
                    {"kind": "repo", "id": "r2"},
                    {"kind": "workspace", "id": "w1"},
                    {"kind": "repo", "id": "r1"},
                ],
            })
        );

        let sh = of_kind(&repos, ContainerKind::Shell).key.clone();
        let detached = of_kind(&repos, ContainerKind::Detached).key.clone();
        for key in [sh, detached] {
            assert!(
                sidebar_reorder(&r2, &repos, &header(&key), Side::Before).is_none(),
                "{key} takes no drop"
            );
        }
        assert!(
            sidebar_reorder(&r2, &repos, &header("repo:r2"), Side::After).is_none(),
            "a container dropped on itself sends nothing"
        );
        assert!(
            sidebar_reorder(&r2, &repos, &header("repo:r1"), Side::After).is_none(),
            "a drop that keeps the order sends nothing"
        );
        assert!(
            sidebar_reorder(&r2, &repos, &leaf_row("repo:r1", "a"), Side::Before).is_none(),
            "a container drops on a header only"
        );
    }

    #[test]
    fn tab_container_drop_reorders_the_tabs() {
        let tabs = tabs_view();
        let t2 = DragPayload::Tab("t2".to_owned());

        let message =
            sidebar_reorder(&t2, &tabs, &header("tab:t1"), Side::Before).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_tabs", "ordered_ids": ["t2", "t1"]})
        );

        let unbound = of_kind(&tabs, ContainerKind::Unbound).key.clone();
        assert!(sidebar_reorder(&t2, &tabs, &header(&unbound), Side::Before).is_none());
        assert!(
            sidebar_reorder(&t2, &repos_view(), &header("repo:r1"), Side::Before).is_none(),
            "a tab drags onto tab containers only"
        );
    }

    #[test]
    fn leaf_drop_reorders_within_its_container() {
        let repos = repos_view();
        let message = sidebar_reorder(
            &leaf_drag("c", "r1"),
            &repos,
            &leaf_row("repo:r1", "a"),
            Side::Before,
        )
        .expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({
                "type": "reorder_sessions",
                "container_id": "r1",
                "ordered_ids": ["c", "a", "b"],
            }),
            "the whole shown order of the container"
        );

        assert!(
            sidebar_reorder(
                &leaf_drag("c", "r1"),
                &repos,
                &leaf_row("repo:r2", "d"),
                Side::Before
            )
            .is_none(),
            "a leaf dropped in another container does nothing"
        );
        assert!(
            sidebar_reorder(
                &leaf_drag("c", "r1"),
                &repos,
                &header("repo:r1"),
                Side::Before
            )
            .is_none(),
            "a leaf drops on leaves only"
        );

        let tabs = tabs_view();
        let message = sidebar_reorder(
            &leaf_drag("b", "t1"),
            &tabs,
            &leaf_row("tab:t1", "a"),
            Side::Before,
        )
        .expect("a reorder in a tab container");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_sessions", "container_id": "t1", "ordered_ids": ["b", "a"]})
        );
    }

    #[test]
    fn reorder_sessions_sends_nothing_when_the_order_stays() {
        let ids: Vec<String> = ["a", "b"].iter().map(|id| (*id).to_owned()).collect();

        assert!(reorder_sessions(&ids, "r1", "a", "a", Side::After).is_none());
        assert!(reorder_sessions(&ids, "r1", "a", "b", Side::Before).is_none());
        let message = reorder_sessions(&ids, "r1", "a", "b", Side::After).expect("a reorder");
        assert_eq!(
            wire(&message),
            json!({"type": "reorder_sessions", "container_id": "r1", "ordered_ids": ["b", "a"]})
        );
    }

    #[test]
    fn side_of_uses_the_midpoint() {
        assert_eq!(side_of(4.0, 0.0, 20.0), Side::Before);
        assert_eq!(side_of(16.0, 0.0, 20.0), Side::After);
        assert_eq!(
            side_of(10.0, 0.0, 20.0),
            Side::After,
            "a pointer exactly at the middle counts as the second half"
        );
        assert_eq!(
            side_of(105.0, 100.0, 20.0),
            Side::Before,
            "the half is measured from the row's own start"
        );
    }

    #[test]
    fn pane_drop_builds_the_move_pane_message() {
        let tabs = tabs();

        let drop = pane_drop(
            &tabs,
            &drag("t1", "a"),
            "t2",
            "c",
            PaneDropEdge::Left,
            Some("r7".to_owned()),
        )
        .expect("a cross-tab move");

        assert_eq!(drop.src_tab_id, "t1");
        assert_eq!(drop.dst_tab_id, "t2");
        assert_eq!(
            wire(&drop.message()),
            json!({
                "type": "move_pane",
                "src_tab_id": "t1",
                "src_pane_id": "a",
                "dst_tab_id": "t2",
                "dst_pane_id": "c",
                "edge": "left",
                "touch": true,
                "request_id": "r7",
            })
        );
    }

    #[test]
    fn outer_drop_keeps_the_hovered_pane_id() {
        let tabs = tabs();

        let drop = dropped(&tabs, &drag("t1", "a"), "t2", "c", PaneDropEdge::OuterLeft)
            .expect("an outer drop");

        assert!(
            matches!(
                &drop.message(),
                ClientMessage::MovePane {
                    dst_pane_id,
                    edge: PaneDropEdge::OuterLeft,
                    ..
                } if dst_pane_id == "c"
            ),
            "{:?}",
            drop.message()
        );
    }

    #[test]
    fn drop_on_a_diff_tab_gives_nothing() {
        let tabs = tabs();

        assert!(
            dropped(&tabs, &drag("t1", "a"), "t3", "p1", PaneDropEdge::Left).is_none(),
            "a diff tab has no grid to drop into"
        );
    }

    #[test]
    fn drop_on_a_missing_tab_gives_nothing() {
        let tabs = tabs();

        assert!(dropped(&tabs, &drag("t1", "a"), "gone", "c", PaneDropEdge::Left).is_none());
        assert!(
            dropped(&tabs, &drag("gone", "a"), "t2", "c", PaneDropEdge::Left).is_none(),
            "a payload from a tab that is gone sends nothing"
        );
    }

    #[test]
    fn non_pane_payloads_give_nothing() {
        let tabs = tabs();
        let payloads = [
            DragPayload::Tab("t1".to_owned()),
            DragPayload::Container(ContainerRef::Repo("r1".to_owned())),
            DragPayload::Leaf {
                session_id: "s1".to_owned(),
                container_id: "r1".to_owned(),
            },
        ];

        for payload in payloads {
            assert!(
                dropped(&tabs, &payload, "t2", "c", PaneDropEdge::Left).is_none(),
                "a pill over a pane sends no move: {payload:?}"
            );
        }
    }

    #[test]
    fn an_empty_pane_still_drags() {
        let tabs = vec![
            tab("t1", &split(H, 0.5, pane("a", None), pane("b", Some("s2")))),
            tab(
                "t2",
                &split(V, 0.5, pane("c", Some("s3")), pane("d", Some("s4"))),
            ),
        ];

        let drop = dropped(&tabs, &drag("t1", "a"), "t2", "d", PaneDropEdge::Right)
            .expect("an empty pane drags like any other");
        assert_eq!(drop.undo_label, undo::MOVED_PANE);
    }
}
