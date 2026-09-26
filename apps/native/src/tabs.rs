//! The tab strip and split panes as a plain model: the tabs the daemon keeps
//! for this client, the active tab, each tab's focused pane, where a session
//! is placed, and where a tab's panes and dividers sit.

use std::collections::{HashMap, HashSet};

use protocol::{
    ClientMessage, DaemonMessage, GridNode, SessionSnapshot, SplitDirection, SplitPlace, TabEntry,
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

/// The tab list, the active tab and each tab's focused pane.
#[derive(Debug, Default)]
pub struct TabsModel {
    tabs: Vec<TabEntry>,
    loaded: bool,
    active: Option<String>,
    /// The active tab saved by the last run, used once on the first list.
    restored_active: Option<String>,
    focused: HashMap<String, String>,
    /// New tabs this client asked for that become active when they arrive.
    pending_create: u32,
    /// A pane the view should give the keyboard to.
    focus_request: Option<String>,
    pub close_confirm: CloseConfirm,
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

/// Whether closing `tab` loses something worth a second click: a bound
/// session, or a layout of two panes or more.
pub fn needs_confirm(tab: &TabEntry) -> bool {
    tab.grid().is_some_and(|grid| {
        let panes = collect_panes(grid);
        panes.len() >= 2 || panes.iter().any(|p| p.session.is_some())
    })
}

/// The pane that sizes each session's PTY, by session: the focused pane
/// when it shows the session, else the first pane of the active tab that
/// does. Hidden tabs drive nothing.
pub fn size_drivers(
    bindings: &[PaneBinding],
    active_tab: Option<&str>,
    focused_pane: Option<&str>,
) -> HashMap<String, String> {
    let mut drivers = HashMap::new();
    let visible: Vec<&PaneBinding> = bindings
        .iter()
        .filter(|b| Some(b.tab_id.as_str()) == active_tab)
        .collect();
    let focused = visible
        .iter()
        .filter(|b| Some(b.pane_id.as_str()) == focused_pane);
    for binding in focused.chain(visible.iter()) {
        if let Some(session_id) = &binding.session_id {
            drivers
                .entry(session_id.clone())
                .or_insert_with(|| binding.pane_id.clone());
        }
    }
    drivers
}

impl Placement {
    /// The request that puts `session_id` there.
    pub fn message(&self, session_id: &str) -> ClientMessage {
        let session_id = Some(session_id.to_owned());
        match self {
            Self::NewTab => ClientMessage::CreateTab {
                name: None,
                initial_session_id: session_id,
            },
            Self::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => ClientMessage::ReplacePaneSession {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
                session_id,
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

impl TabsModel {
    pub fn new(restored_active: Option<String>) -> Self {
        Self {
            restored_active,
            ..Self::default()
        }
    }

    /// Folds a tab message in; returns whether it was one.
    pub fn apply(&mut self, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::Tabs { tabs } => self.replace_all(tabs),
            DaemonMessage::TabUpdated { tab } => self.upsert(tab),
            DaemonMessage::TabRemoved { tab_id } => self.remove(tab_id),
            DaemonMessage::TabsReordered { ordered_ids } => self.reorder(ordered_ids),
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
        self.focused
            .retain(|tab_id, _| live.contains(tab_id.as_str()));
        let active = self
            .active
            .take()
            .or_else(|| self.restored_active.take())
            .filter(|id| live.contains(id.as_str()))
            .or_else(|| self.tabs.first().map(|t| t.id.clone()));
        self.activate_id(active);
    }

    /// A tab created or changed. A new tab this client asked for, or the
    /// first tab of all, becomes active.
    fn upsert(&mut self, tab: &TabEntry) {
        if let Some(slot) = self.tabs.iter_mut().find(|t| t.id == tab.id) {
            let old = std::mem::replace(slot, tab.clone());
            self.focus_after_update(&old, tab);
            return;
        }
        self.tabs.push(tab.clone());
        if self.pending_create > 0 || self.active.is_none() {
            self.pending_create = self.pending_create.saturating_sub(1);
            self.activate_id(Some(tab.id.clone()));
        }
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

    /// A closed tab; when it was active, the first tab takes over.
    fn remove(&mut self, tab_id: &str) {
        self.tabs.retain(|t| t.id != tab_id);
        self.focused.remove(tab_id);
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

    /// The next new tab to arrive is one this client asked for.
    pub fn arm_create(&mut self) {
        self.pending_create = self.pending_create.saturating_add(1);
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
    /// a new tab when `tab_id` is gone or holds no grid.
    pub fn place_in(
        &self,
        tab_id: &str,
        session: &SessionSnapshot,
        sessions: &[SessionSnapshot],
    ) -> Placement {
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
        }
    }

    fn split(direction: SplitDirection, ratio: f32, first: GridNode, second: GridNode) -> GridNode {
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

    fn diff_tab(id: &str) -> TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "diff", "repo_id": "r", "path": "a.rs", "against": null },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("diff tab fixture")
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
            size_drivers(&bindings, active, focused)
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
        assert!(size_drivers(&bindings, None, None).is_empty());
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
            wire(&Placement::NewTab.message("new")),
            wire(&ClientMessage::CreateTab {
                name: None,
                initial_session_id: Some("new".to_owned()),
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
            wire(&placement.message("new")),
            wire(&ClientMessage::ReplacePaneSession {
                tab_id: "t1".to_owned(),
                pane_id: "e".to_owned(),
                session_id: Some("new".to_owned()),
            })
        );
        let split = Placement::Pane {
            tab_id: "t1".to_owned(),
            target: split_target("e", V),
        };
        assert_eq!(
            wire(&split.message("new")),
            wire(&ClientMessage::SplitPane {
                tab_id: "t1".to_owned(),
                pane_id: "e".to_owned(),
                direction: V,
                place: SplitPlace::Second,
                new_session_id: Some("new".to_owned()),
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
    fn tabs_reorder_follows_the_daemon() {
        let mut model = model_with(&[tab("t1", &pane("a", None)), tab("t2", &pane("b", None))]);
        model.apply(&DaemonMessage::TabsReordered {
            ordered_ids: vec!["t2".to_owned(), "t1".to_owned()],
        });
        let ids: Vec<&str> = model.tabs().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["t2", "t1"]);
    }

    #[test]
    fn tabs_a_requested_tab_becomes_active_once() {
        let mut model = model_with(&[tab("t1", &pane("a", None))]);
        model.apply(&updated(&tab("t2", &pane("b", None))));
        assert_eq!(model.active_id(), Some("t1"), "another client's tab");
        model.arm_create();
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
}
