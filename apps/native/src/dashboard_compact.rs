//! The Dashboard's Compact view model, laid out as stacked lanes: one block
//! per sidebar container that lists a session, in the sidebar's order, each
//! block a row per live session with the session's agent tree as chips.
//! Ended sessions get no row, only a count in their block's fold row; past
//! the measured height, the repo with the most idle sessions with nothing
//! running folds them into that row too, until the blocks fit.
//!
//! Plain Rust over the Dashboard's row texts, so the widths, the height
//! estimate and the fold are unit-testable without GPUI; nothing here draws.

use chrono::{DateTime, Utc};
use gpui::px;
use protocol::{AgentKind, AgentNode, AgentState, SessionStatus};

use crate::dashboard::GroupKind;
use crate::dashboard_rows::{
    DASH, RowInput, format_age, issue_key, now_text, state_word, stuck_flags,
};
use crate::fonts::ui_px_at;
use crate::sidebar::ContainerKind;

/// What the view says with no agent listed.
pub const COMPACT_EMPTY_TEXT: &str = "No agents running.";
/// The view's text size, before the UI text-size offset.
pub(crate) const TEXT_PX: f32 = 12.0;
/// A session row's height, and the fold row's, before the offset; fixed so a block fits without scrolling.
pub(crate) const ROW_H: f32 = 30.0;
/// The repo cell's width on the left of a block.
pub(crate) const REPO_W: f32 = 150.0;
/// The view's padding around its blocks, across and down.
pub(crate) const BODY_PAD_X: f32 = 10.0;
pub(crate) const BODY_PAD_Y: f32 = 7.0;
/// The gap between two blocks.
pub(crate) const BLOCK_GAP: f32 = 5.0;
/// A chip's height (before the offset), the gaps between chips and the chips
/// cell's padding; fixed so every chip's row lines up.
pub(crate) const CHIP_H: f32 = 22.0;
pub(crate) const CHIP_GAP_X: f32 = 4.0;
pub(crate) const CHIP_GAP_Y: f32 = 3.0;
pub(crate) const CHIPS_PAD_X: f32 = 6.0;
pub(crate) const CHIPS_PAD_Y: f32 = 3.0;
/// The session cell's width at its widest and at its narrowest; capped so the view never scrolls.
const SESSION_MAX_W: f32 = 642.0;
const SESSION_MIN_W: f32 = 360.0;
/// The room the session cell leaves the chips while it can.
const CHIPS_ROOM_W: f32 = 260.0;
/// Under this much room beside the narrowest session cell, the chips move
/// under it.
const CHIPS_MIN_W: f32 = 200.0;
/// The session cell widths under which the issue key, then the age, drop.
const ISSUE_MIN_W: f32 = 560.0;
const AGE_MIN_W: f32 = 470.0;
/// The repo cell's three line heights (name, tag, tally) before the offset,
/// its padding down and the gap between its lines; a block is never shorter
/// than the cell.
pub(crate) const REPO_LINES: [f32; 3] = [17.0, 14.0, 16.0];
pub(crate) const REPO_PAD_Y: f32 = 7.0;
pub(crate) const REPO_LINE_GAP: f32 = 3.0;
/// A block's two 1 px borders, across or down.
pub(crate) const BLOCK_EDGES: f32 = 2.0;
/// The 1 px border over every row after a block's first.
const ROW_EDGE: f32 = 1.0;
/// A chip's estimate: padding, glyph, gap and padding around its text, a
/// kind icon with its gap, its two 1 px borders, and one character of text
/// at [`TEXT_PX`].
const CHIP_PAD_L: f32 = 6.0;
const CHIP_GLYPH_W: f32 = 10.0;
const CHIP_GLYPH_GAP: f32 = 5.0;
const CHIP_PAD_R: f32 = 7.0;
const CHIP_ICON_W: f32 = 16.0;
const CHIP_EDGES: f32 = 2.0;
const CHAR_W: f32 = 7.0;

/// The sizes that follow the UI text-size offset, as `ui_px` scales them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CompactSizes {
    /// A session row's height, and the fold row's.
    pub(crate) row_h: f32,
    pub(crate) chip_h: f32,
    /// The repo cell's line heights.
    pub(crate) repo_lines: [f32; 3],
    /// One character's estimated width.
    char_w: f32,
}

impl CompactSizes {
    /// The sizes at UI text-size offset `offset`.
    pub(crate) fn at(offset: i8) -> Self {
        let scaled = |base: f32| ui_px_at(offset, base) / px(1.0);
        Self {
            row_h: scaled(ROW_H),
            chip_h: scaled(CHIP_H),
            repo_lines: REPO_LINES.map(scaled),
            char_w: CHAR_W * scaled(TEXT_PX) / TEXT_PX,
        }
    }

    /// The repo cell's height: its padding, lines and the gaps between them.
    fn repo_cell_h(&self) -> f32 {
        let lines: f32 = self.repo_lines.iter().sum();
        2.0 * REPO_PAD_Y + 2.0 * REPO_LINE_GAP + lines
    }
}

/// What a chip stands for, which sets its icon and its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    /// A subagent, or a node of a kind this build does not know.
    Agent,
    /// A background shell, monitor or gate: a terminal prompt icon.
    Shell,
    /// A dispatch run: an up-right arrow in the workspace colour.
    Dispatch,
}

/// One node of a session's agent tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactChip {
    pub key: String,
    pub kind: ChipKind,
    pub state: AgentState,
    pub name: String,
    /// What it runs now, or how it ended.
    pub what: String,
}

impl CompactChip {
    /// Whether the chip draws a kind icon before its name.
    #[must_use]
    pub fn has_icon(&self) -> bool {
        self.kind != ChipKind::Agent
    }

    /// The chip's estimated width at `char_w` px a character, its borders
    /// included, at most `cap`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a chip's text is far shorter than f32's exact integer range"
    )]
    fn estimated_w(&self, char_w: f32, cap: f32) -> f32 {
        let chars = self.name.chars().count() + 1 + self.what.chars().count();
        let icon = if self.has_icon() { CHIP_ICON_W } else { 0.0 };
        let width = CHIP_PAD_L
            + CHIP_GLYPH_W
            + CHIP_GLYPH_GAP
            + icon
            + char_w * chars as f32
            + CHIP_PAD_R
            + CHIP_EDGES;
        width.min(cap)
    }
}

/// One live session's row: its session cell's texts and its chips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactRow {
    pub session_id: String,
    pub label: String,
    /// The state word, e.g. `Working` or `Question`.
    pub state: String,
    /// The now line; empty when it would only repeat the state word.
    pub now: String,
    /// The stuck flags drawn before the now line.
    pub stuck: Vec<String>,
    /// The issue key; empty when the session has none.
    pub issue: String,
    /// How long the status has held; `None` without a stamp.
    pub age: Option<String>,
    /// The row waits on the user: washed.
    pub asking: bool,
    /// The session's agent tree, depth first.
    pub chips: Vec<CompactChip>,
    pub(crate) kind: GroupKind,
}

impl CompactRow {
    /// Idle, with nothing of its own still running: a fold may hide it.
    fn foldable(&self) -> bool {
        self.kind == GroupKind::Idle
            && !self
                .chips
                .iter()
                .any(|chip| chip.state == AgentState::Running)
    }
}

/// A block's sessions by state, as its repo cell counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub asking: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

/// One sidebar container's block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBlock {
    /// The container's place in the sidebar's order.
    pub index: usize,
    pub name: String,
    pub(crate) kind: ContainerKind,
    /// The rows drawn, in the sidebar's order.
    pub rows: Vec<CompactRow>,
    /// The labels of the idle sessions a fold hides.
    pub folded: Vec<String>,
    /// How many of its listed sessions ended.
    pub ended: usize,
    pub tally: Tally,
}

impl CompactBlock {
    pub(crate) fn new(index: usize, name: String, kind: ContainerKind) -> Self {
        Self {
            index,
            name,
            kind,
            rows: Vec::new(),
            folded: Vec::new(),
            ended: 0,
            tally: Tally::default(),
        }
    }

    /// Takes the session of `input`, listed in group `kind`: an ended one
    /// joins the ended count, any other gets its row and its tally.
    pub(crate) fn add(&mut self, input: &RowInput<'_>, kind: GroupKind, now: DateTime<Utc>) {
        if matches!(
            input.session.status,
            SessionStatus::Stopped | SessionStatus::Error
        ) {
            self.ended += 1;
            return;
        }
        let count = match kind {
            GroupKind::NeedsYou => &mut self.tally.asking,
            GroupKind::Working => &mut self.tally.working,
            GroupKind::Done => &mut self.tally.done,
            GroupKind::Idle => &mut self.tally.idle,
        };
        *count += 1;
        self.rows.push(row_of(input, kind, now));
    }

    /// Whether the block holds nothing to draw.
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.ended == 0
    }

    /// Whether the container is a workspace.
    #[must_use]
    pub fn is_workspace(&self) -> bool {
        self.kind == ContainerKind::Workspace
    }

    /// The caps tag under the name.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self.kind {
            ContainerKind::Workspace => "WORKSPACE",
            ContainerKind::Repo => "REPO",
            _ => "FOLDER",
        }
    }

    /// The live sessions the block stands for, folded ones included.
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.tally.asking + self.tally.working + self.tally.done + self.tally.idle
    }

    /// The fold row's idle chip, `+N idle`, while a fold hides some.
    #[must_use]
    pub fn idle_text(&self) -> Option<String> {
        (!self.folded.is_empty()).then(|| format!("+{} idle", self.folded.len()))
    }

    /// The fold row's ended count, `+N ended`, while some ended.
    #[must_use]
    pub fn ended_text(&self) -> Option<String> {
        (self.ended > 0).then(|| format!("+{} ended", self.ended))
    }

    /// Whether the block ends in a fold row.
    #[must_use]
    pub fn has_fold_row(&self) -> bool {
        !self.folded.is_empty() || self.ended > 0
    }

    fn foldable(&self) -> usize {
        self.rows.iter().filter(|row| row.foldable()).count()
    }

    /// Moves the foldable rows into the fold row.
    fn fold(&mut self) {
        let (hidden, shown): (Vec<CompactRow>, Vec<CompactRow>) =
            self.rows.drain(..).partition(CompactRow::foldable);
        self.rows = shown;
        self.folded.extend(hidden.into_iter().map(|row| row.label));
    }
}

/// The row `input` draws, listed in group `kind`, at `now`.
fn row_of(input: &RowInput<'_>, kind: GroupKind, now: DateTime<Utc>) -> CompactRow {
    let facts = input.facts;
    let issue = issue_key(facts);
    let state = state_word(input);
    let now_line = now_text(input);
    CompactRow {
        session_id: input.row.session_id.clone(),
        label: input.row.label.clone(),
        now: if now_line.eq_ignore_ascii_case(&state) {
            String::new()
        } else {
            now_line
        },
        state,
        stuck: facts.map_or_else(Vec::new, |facts| stuck_flags(&facts.stuck)),
        issue: if issue == DASH { String::new() } else { issue },
        age: input
            .session
            .status_since
            .map(|since| format_age(since, now)),
        asking: kind == GroupKind::NeedsYou,
        chips: facts.map_or_else(Vec::new, |facts| chips_of(&facts.agents)),
        kind,
    }
}

/// The chips of an agent tree, depth first, as By status nests its rows.
fn chips_of(nodes: &[AgentNode]) -> Vec<CompactChip> {
    let mut chips = Vec::new();
    push_chips(nodes, &mut chips);
    chips
}

fn push_chips(nodes: &[AgentNode], chips: &mut Vec<CompactChip>) {
    for node in nodes {
        chips.push(CompactChip {
            key: node.key.clone(),
            kind: chip_kind(node.kind),
            state: node.state,
            name: node.name.clone(),
            what: node.task.clone(),
        });
        push_chips(&node.children, chips);
    }
}

fn chip_kind(kind: AgentKind) -> ChipKind {
    match kind {
        AgentKind::Subagent | AgentKind::Unknown => ChipKind::Agent,
        AgentKind::Shell | AgentKind::Monitor | AgentKind::Gate => ChipKind::Shell,
        AgentKind::Dispatch => ChipKind::Dispatch,
    }
}

/// How a row lays out across a view of a measured width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CompactLayout {
    /// The session cell's width.
    pub(crate) session_w: f32,
    /// The chips cell's share of the row, padding included; the block's
    /// borders take [`BLOCK_EDGES`] of it.
    pub(crate) chips_w: f32,
    /// The chips sit under the session cell rather than beside it.
    pub(crate) stacked: bool,
    /// The session cell draws the issue key, and the age.
    pub(crate) issue: bool,
    pub(crate) age: bool,
    /// The heights and the character width at the UI text-size offset.
    pub(crate) sizes: CompactSizes,
}

/// The layout of a view `view_w` pixels wide at UI text-size offset
/// `offset`: the session cell takes up to 642 px while the chips keep 260,
/// never under 360; with under 200 px beside that, the chips move under it
/// and it takes the row.
pub(crate) fn layout(view_w: f32, offset: i8) -> CompactLayout {
    let rows_w = (view_w - 2.0 * BODY_PAD_X - REPO_W).max(0.0);
    let stacked = rows_w - SESSION_MIN_W < CHIPS_MIN_W;
    let (session_w, chips_w) = if stacked {
        (rows_w, rows_w)
    } else {
        let session_w = (rows_w - CHIPS_ROOM_W).clamp(SESSION_MIN_W, SESSION_MAX_W);
        (session_w, rows_w - session_w)
    };
    CompactLayout {
        session_w,
        chips_w,
        stacked,
        issue: session_w >= ISSUE_MIN_W,
        age: session_w >= AGE_MIN_W,
        sizes: CompactSizes::at(offset),
    }
}

/// The room a chips cell's share `chips_w` leaves its chips on a line: less
/// the block's borders and the cell's padding.
pub(crate) fn chips_room(chips_w: f32) -> f32 {
    (chips_w - BLOCK_EDGES - 2.0 * CHIPS_PAD_X).max(0.0)
}

/// How many lines `chips` take, packed greedily into a chips cell whose
/// share of the row is `chips_w`, at `char_w` px a character.
fn chip_lines(chips: &[CompactChip], chips_w: f32, char_w: f32) -> usize {
    let room = chips_room(chips_w);
    let mut lines = 0;
    let mut used = 0.0;
    for chip in chips {
        let chip_w = chip.estimated_w(char_w, room);
        if lines == 0 || used + CHIP_GAP_X + chip_w > room {
            lines += 1;
            used = chip_w;
        } else {
            used += CHIP_GAP_X + chip_w;
        }
    }
    lines
}

/// The estimated height of a chips cell whose share of the row is `chips_w`
/// holding `chips`: its lines, the gaps between them and its padding, or
/// nothing without a chip.
#[expect(
    clippy::cast_precision_loss,
    reason = "a session's chip lines are far fewer than f32's exact integer range"
)]
pub(crate) fn chips_h(chips: &[CompactChip], chips_w: f32, sizes: CompactSizes) -> f32 {
    let lines = chip_lines(chips, chips_w, sizes.char_w);
    if lines == 0 {
        return 0.0;
    }
    let lines = lines as f32;
    lines * sizes.chip_h + (lines - 1.0) * CHIP_GAP_Y + 2.0 * CHIPS_PAD_Y
}

/// The estimated height of `row` in `layout`, without the border over it.
pub(crate) fn row_h(row: &CompactRow, layout: CompactLayout) -> f32 {
    let row_h = layout.sizes.row_h;
    let chips = chips_h(&row.chips, layout.chips_w, layout.sizes);
    if layout.stacked {
        row_h + chips
    } else {
        row_h.max(chips)
    }
}

/// The estimated height of `block` in `layout`: its rows, each after the
/// first under a 1 px border, and its fold row, or its repo cell when that
/// is taller, inside the block's borders.
pub(crate) fn block_h(block: &CompactBlock, layout: CompactLayout) -> f32 {
    let rows: f32 = block
        .rows
        .iter()
        .enumerate()
        .map(|(at, row)| row_h(row, layout) + if at == 0 { 0.0 } else { ROW_EDGE })
        .sum();
    let fold = if block.has_fold_row() {
        layout.sizes.row_h
    } else {
        0.0
    };
    (rows + fold).max(layout.sizes.repo_cell_h()) + BLOCK_EDGES
}

/// The estimated height of the view drawing `blocks` in `layout`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a view's blocks are far fewer than f32's exact integer range"
)]
pub(crate) fn view_h(blocks: &[CompactBlock], layout: CompactLayout) -> f32 {
    let gaps = blocks.len().saturating_sub(1) as f32 * BLOCK_GAP;
    let heights: f32 = blocks.iter().map(|block| block_h(block, layout)).sum();
    2.0 * BODY_PAD_Y + heights + gaps
}

/// Folds repos until `blocks` fit `height` in `layout`: each time the one
/// with the most foldable sessions, at least two, the first in the sidebar
/// on a tie. What still does not fit clips.
pub(crate) fn fit(blocks: &mut [CompactBlock], layout: CompactLayout, height: f32) {
    while view_h(blocks, layout) > height {
        let mut next: Option<(usize, usize)> = None;
        for (at, block) in blocks.iter().enumerate() {
            let count = block.foldable();
            if count >= 2 && next.is_none_or(|(_, most)| count > most) {
                next = Some((at, count));
            }
        }
        let Some((at, _)) = next else {
            return;
        };
        blocks[at].fold();
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails on the panic of a broken fixture"
)]
mod tests {
    use protocol::{SessionFacts, SessionSnapshot};
    use serde_json::{Value, json};

    use super::*;
    use crate::dashboard::DashboardRow;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp")
    }

    fn session(id: &str, status: SessionStatus) -> SessionSnapshot {
        let mut session: SessionSnapshot = serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [{
                "repo_id": "r1",
                "repo_name": "r1",
                "branch": "main",
                "worktree_path": "",
            }],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture");
        session.status = status;
        session
    }

    fn facts_with(id: &str, agents: &Value) -> SessionFacts {
        serde_json::from_value(json!({
            "session_id": id,
            "now": { "kind": "unknown", "text": "" },
            "agents": agents,
        }))
        .expect("facts fixture")
    }

    fn node(key: &str, kind: &str, state: &str, name: &str, task: &str) -> Value {
        json!({
            "key": key, "kind": kind, "name": name, "task": task, "state": state,
            "started_at": "2026-01-01T00:00:00Z",
        })
    }

    fn add(
        block: &mut CompactBlock,
        id: &str,
        (status, kind): (SessionStatus, GroupKind),
        facts: Option<&SessionFacts>,
    ) {
        let session = session(id, status);
        let row = DashboardRow {
            session_id: id.to_owned(),
            container_name: block.name.clone(),
            label: id.to_owned(),
            busy_excluded: false,
        };
        let input = RowInput {
            row: &row,
            session: &session,
            facts,
            agents: &[],
            unseen: false,
            prompt: None,
        };
        block.add(&input, kind, at());
    }

    const ASKING: (SessionStatus, GroupKind) = (SessionStatus::AwaitingInput, GroupKind::NeedsYou);
    const WORKING: (SessionStatus, GroupKind) = (SessionStatus::Working, GroupKind::Working);
    const IDLE: (SessionStatus, GroupKind) = (SessionStatus::Idle, GroupKind::Idle);
    const DONE: (SessionStatus, GroupKind) = (SessionStatus::Idle, GroupKind::Done);

    /// A repo block `index` named `name` with `idle` idle sessions.
    fn idle_block(index: usize, name: &str, idle: usize) -> CompactBlock {
        let mut block = CompactBlock::new(index, name.to_owned(), ContainerKind::Repo);
        for n in 0..idle {
            add(&mut block, &format!("{name}-idle-{n}"), IDLE, None);
        }
        block
    }

    fn ids(block: &CompactBlock) -> Vec<&str> {
        block
            .rows
            .iter()
            .map(|row| row.session_id.as_str())
            .collect()
    }

    fn wide() -> CompactLayout {
        layout(1588.0, 0)
    }

    #[test]
    fn a_block_keeps_the_order_its_sessions_were_added_in_and_tallies_them() {
        let mut block = CompactBlock::new(3, "yaat".to_owned(), ContainerKind::Repo);
        add(&mut block, "w", WORKING, None);
        add(&mut block, "a", ASKING, None);
        add(&mut block, "i", IDLE, None);
        add(&mut block, "d", DONE, None);
        add(&mut block, "w2", WORKING, None);
        assert_eq!(ids(&block), ["w", "a", "i", "d", "w2"]);
        assert_eq!(
            block.tally,
            Tally {
                asking: 1,
                working: 2,
                done: 1,
                idle: 1
            }
        );
        assert_eq!(block.sessions(), 5);
        assert_eq!(block.tag(), "REPO");
        assert!(block.rows[1].asking, "the waiting row is washed");
        assert!(!block.rows[0].asking);
        assert!(!block.has_fold_row(), "nothing folds or ended");
        let workspace = CompactBlock::new(0, "ops".to_owned(), ContainerKind::Workspace);
        assert!(workspace.is_workspace());
        assert_eq!(workspace.tag(), "WORKSPACE");
        assert!(workspace.is_empty(), "a block with nothing listed");
    }

    #[test]
    fn ended_sessions_become_the_ended_count() {
        let mut block = CompactBlock::new(0, "r1".to_owned(), ContainerKind::Repo);
        add(&mut block, "w", WORKING, None);
        add(
            &mut block,
            "stopped",
            (SessionStatus::Stopped, GroupKind::Done),
            None,
        );
        add(
            &mut block,
            "errored",
            (SessionStatus::Error, GroupKind::Done),
            None,
        );
        assert_eq!(ids(&block), ["w"], "an ended session has no row");
        assert_eq!(block.ended_text().as_deref(), Some("+2 ended"));
        assert_eq!(block.idle_text(), None);
        assert!(block.has_fold_row());
        assert_eq!(block.sessions(), 1, "the tally counts live sessions");
        let mut only_ended = CompactBlock::new(1, "r2".to_owned(), ContainerKind::Repo);
        add(
            &mut only_ended,
            "gone",
            (SessionStatus::Stopped, GroupKind::Done),
            None,
        );
        assert!(!only_ended.is_empty(), "its fold row still draws");
    }

    #[test]
    fn a_row_reads_its_texts_and_chips_depth_first() {
        let facts = facts_with(
            "w",
            &json!([
                {
                    "key": "subagent:1", "kind": "subagent", "name": "implementer",
                    "task": "editing footer.rs", "state": "running",
                    "started_at": "2026-01-01T00:00:00Z",
                    "children": [node("subagent:2", "subagent", "finished", "Explore", "mapped")],
                },
                node("bg:1", "shell", "failed", "gate: clippy", "2 warnings"),
                node("gate:1", "gate", "running", "gate: nextest", "41%"),
                node("dispatch:1", "dispatch", "queued", "deepseek: RT-771", "queued"),
            ]),
        );
        let mut block = CompactBlock::new(0, "r1".to_owned(), ContainerKind::Repo);
        add(&mut block, "w", WORKING, Some(&facts));
        let row = &block.rows[0];
        assert_eq!(row.state, "Working");
        assert_eq!(row.now, "Thinking");
        assert_eq!(row.issue, "", "no key reads empty, never a dash");
        assert_eq!(row.age, None, "no status stamp");
        assert_eq!(
            row.chips
                .iter()
                .map(|chip| (chip.key.as_str(), chip.kind, chip.state))
                .collect::<Vec<_>>(),
            [
                ("subagent:1", ChipKind::Agent, AgentState::Running),
                ("subagent:2", ChipKind::Agent, AgentState::Finished),
                ("bg:1", ChipKind::Shell, AgentState::Failed),
                ("gate:1", ChipKind::Shell, AgentState::Running),
                ("dispatch:1", ChipKind::Dispatch, AgentState::Queued),
            ]
        );
        assert_eq!(row.chips[0].what, "editing footer.rs");
        assert!(!row.chips[0].has_icon() && row.chips[2].has_icon() && row.chips[4].has_icon());
    }

    #[test]
    fn the_fold_picks_the_repo_with_the_most_foldable_sessions_first() {
        let mut blocks = vec![
            idle_block(0, "a", 2),
            idle_block(1, "b", 4),
            idle_block(2, "c", 3),
        ];
        let tall = view_h(&blocks, wide());
        fit(&mut blocks, wide(), tall);
        assert!(
            blocks.iter().all(|block| block.folded.is_empty()),
            "an exact fit folds nothing"
        );
        fit(&mut blocks, wide(), tall - 1.0);
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.folded.len())
                .collect::<Vec<_>>(),
            [0, 4, 0],
            "only as much folds as the height needs, the most first"
        );
        assert_eq!(blocks[1].rows.len(), 0);
        assert_eq!(blocks[1].idle_text().as_deref(), Some("+4 idle"));
        assert_eq!(blocks[1].folded[0], "b-idle-0", "labels in order");
        fit(&mut blocks, wide(), 0.0);
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.folded.len())
                .collect::<Vec<_>>(),
            [2, 4, 3],
            "every candidate folds, then the view clips"
        );
    }

    #[test]
    fn a_tie_folds_the_first_repo_and_a_single_idle_session_never_folds() {
        let mut blocks = vec![
            idle_block(0, "a", 1),
            idle_block(1, "b", 3),
            idle_block(2, "c", 3),
        ];
        let tall = view_h(&blocks, wide());
        fit(&mut blocks, wide(), tall - 1.0);
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.folded.len())
                .collect::<Vec<_>>(),
            [0, 3, 0]
        );
        fit(&mut blocks, wide(), 0.0);
        assert_eq!(blocks[0].folded.len(), 0, "one idle session stays a row");
    }

    #[test]
    fn needs_you_working_done_and_running_nodes_never_fold() {
        let shell = facts_with(
            "busy",
            &json!([node("bg:1", "shell", "running", "npm dev", ":5174")]),
        );
        let finished = facts_with(
            "spent",
            &json!([node("bg:2", "shell", "finished", "cleanup", "done")]),
        );
        let mut block = CompactBlock::new(0, "r1".to_owned(), ContainerKind::Repo);
        add(&mut block, "ask", ASKING, None);
        add(&mut block, "work", WORKING, None);
        add(&mut block, "done", DONE, None);
        add(&mut block, "busy", IDLE, Some(&shell));
        add(&mut block, "spent", IDLE, Some(&finished));
        add(&mut block, "calm", IDLE, None);
        let mut blocks = vec![block];
        fit(&mut blocks, wide(), 0.0);
        assert_eq!(ids(&blocks[0]), ["ask", "work", "done", "busy"]);
        assert_eq!(blocks[0].folded, ["spent", "calm"]);
        assert_eq!(blocks[0].tally.idle, 3, "the tally still counts them");
    }

    fn chip(name: &str, what: &str, kind: ChipKind) -> CompactChip {
        CompactChip {
            key: name.to_owned(),
            kind,
            state: AgentState::Running,
            name: name.to_owned(),
            what: what.to_owned(),
        }
    }

    #[test]
    fn chips_pack_greedily_into_lines() {
        // 6 + 10 + 5 + 7 * 9 + 7 + 2 = 93 px; a shell adds its 16 px icon.
        let agent = chip("abcd", "efgh", ChipKind::Agent);
        assert_eq!(agent.estimated_w(CHAR_W, 1000.0), 93.0);
        assert_eq!(
            chip("abcd", "efgh", ChipKind::Shell).estimated_w(CHAR_W, 1000.0),
            109.0
        );
        assert_eq!(agent.estimated_w(CHAR_W, 50.0), 50.0, "capped at the cell");
        let three = vec![agent.clone(), agent.clone(), agent.clone()];
        // The block's 2 px of borders and 12 px of padding: 3 chips need
        // 93 * 3 + 4 * 2 = 287 px of room.
        assert_eq!(chip_lines(&three, 301.0, CHAR_W), 1);
        assert_eq!(chip_lines(&three, 300.0, CHAR_W), 2);
        assert_eq!(chip_lines(&three, 204.0, CHAR_W), 2, "two then one");
        assert_eq!(chip_lines(&three, 203.0, CHAR_W), 3);
        assert_eq!(
            chip_lines(&three, 40.0, CHAR_W),
            3,
            "a chip wider than the cell takes a line"
        );
        assert_eq!(chip_lines(&[], 300.0, CHAR_W), 0);
        let sizes = CompactSizes::at(0);
        assert_eq!(chips_h(&three, 301.0, sizes), 28.0, "one line: 22 + 6");
        assert_eq!(chips_h(&three, 203.0, sizes), 78.0, "three lines, two gaps");
        assert_eq!(chips_h(&[], 301.0, sizes), 0.0, "no chip, no height");
        let larger = CompactSizes::at(3);
        assert_eq!(
            chips_h(&three, 301.0, larger),
            2.0 * 25.0 + 3.0 + 6.0,
            "wider characters wrap sooner, and each line is taller"
        );
    }

    #[test]
    fn a_row_is_as_tall_as_its_chips_and_a_block_as_its_repo_cell() {
        let mut block = CompactBlock::new(0, "r1".to_owned(), ContainerKind::Repo);
        add(&mut block, "calm", IDLE, None);
        let side = wide();
        assert_eq!(row_h(&block.rows[0], side), ROW_H, "no chips, one line");
        assert_eq!(
            block_h(&block, side),
            69.0,
            "the repo cell's padding, gaps and lines, in the block's borders"
        );
        let many: Vec<CompactChip> = (0..12)
            .map(|n| {
                chip(
                    &format!("implementer {n}"),
                    "editing footer.rs",
                    ChipKind::Agent,
                )
            })
            .collect();
        block.rows[0].chips = many;
        let tall = row_h(&block.rows[0], side);
        assert_eq!(
            tall,
            chips_h(&block.rows[0].chips, side.chips_w, side.sizes)
        );
        assert!(tall > ROW_H, "{tall}");
        add(&mut block, "calm2", IDLE, None);
        add(
            &mut block,
            "gone",
            (SessionStatus::Stopped, GroupKind::Done),
            None,
        );
        assert_eq!(
            block_h(&block, side),
            tall + (ROW_H + 1.0) + ROW_H + 2.0,
            "rows, the second under its border, fold row, block borders"
        );
        let narrow = layout(700.0, 0);
        assert!(narrow.stacked);
        assert_eq!(
            row_h(&block.rows[0], narrow),
            ROW_H + chips_h(&block.rows[0].chips, narrow.chips_w, narrow.sizes),
            "stacked chips sit under the session cell"
        );
        assert_eq!(row_h(&block.rows[1], narrow), ROW_H);
        let blocks = vec![block.clone(), block];
        assert_eq!(
            view_h(&blocks, side),
            7.0 + 2.0 * block_h(&blocks[0], side) + 5.0 + 7.0
        );
        let larger = layout(1588.0, 3);
        let calm = idle_block(0, "r2", 2);
        assert_eq!(
            row_h(&calm.rows[0], larger),
            33.0,
            "rows grow with the text"
        );
        assert_eq!(
            block_h(&calm, larger),
            2.0 * 7.0 + 2.0 * 3.0 + (20.0 + 17.0 + 19.0) + 2.0,
            "so does the repo cell's every line"
        );
    }

    #[test]
    fn the_session_cell_narrows_then_drops_the_key_then_the_age() {
        let shape = |w: f32| {
            let l = layout(w, 0);
            (l.session_w, l.stacked, l.issue, l.age)
        };
        assert_eq!(shape(1588.0), (642.0, false, true, true));
        assert_eq!(layout(1588.0, 0).chips_w, 1588.0 - 20.0 - 150.0 - 642.0);
        assert_eq!(shape(1200.0), (642.0, false, true, true));
        assert_eq!(shape(1000.0), (570.0, false, true, true));
        assert_eq!(shape(800.0), (370.0, false, false, false));
        assert_eq!(
            shape(700.0),
            (530.0, true, false, true),
            "the chips go under"
        );
        assert_eq!(layout(700.0, 0).chips_w, 530.0);
        assert!(!shape(989.0).2, "the key drops under 560");
        assert!(shape(990.0).2);
        assert!(!shape(899.0).3, "the age drops under 470");
        assert!(shape(900.0).3);
        assert!(
            !layout(730.0, 0).stacked,
            "200 px beside the narrowest cell"
        );
        assert!(layout(729.0, 0).stacked);
    }
}
