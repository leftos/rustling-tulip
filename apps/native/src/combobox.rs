//! A branch picker's list: which branches a typed value offers, the
//! "Create branch" row, the highlighted row, and whether the list is open.
//! Pure state; `spawn_view` draws it under its field and feeds it keys and
//! clicks.

use std::ops::Range;

use gpui::{Pixels, px};

/// The room kept between the list and the dialog's edge.
const EDGE_GAP: f32 = 4.0;

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComboRow {
    /// A branch on offer.
    Branch(String),
    /// Start a new branch with the typed name.
    Create(String),
}

impl ComboRow {
    /// The text the field takes when the row is committed.
    pub(crate) fn value(&self) -> &str {
        match self {
            Self::Branch(name) | Self::Create(name) => name,
        }
    }

    /// What the row reads.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Branch(name) => name.clone(),
            Self::Create(name) => format!("Create branch “{name}”"),
        }
    }
}

/// A combobox's list over a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Combobox {
    /// Whether a value that matches no branch offers a "Create branch" row.
    allow_create: bool,
    open: bool,
    highlight: usize,
}

impl Combobox {
    pub(crate) fn new(allow_create: bool) -> Self {
        Self {
            allow_create,
            open: false,
            highlight: 0,
        }
    }

    /// The rows `value` offers from `branches`: every branch while the
    /// trimmed value is empty or names one exactly, else those containing
    /// it (case-insensitive); then the create row when allowed and the value
    /// names no branch.
    pub(crate) fn rows(&self, value: &str, branches: &[String]) -> Vec<ComboRow> {
        let trimmed = value.trim();
        let exact = branches.iter().any(|b| b == trimmed);
        let needle = trimmed.to_lowercase();
        let mut rows: Vec<ComboRow> = branches
            .iter()
            .filter(|b| trimmed.is_empty() || exact || b.to_lowercase().contains(&needle))
            .map(|b| ComboRow::Branch(b.clone()))
            .collect();
        if self.allow_create && !trimmed.is_empty() && !exact {
            rows.push(ComboRow::Create(trimmed.to_owned()));
        }
        rows
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the list shows: open, with a row to show.
    pub(crate) fn shown(&self, value: &str, branches: &[String]) -> bool {
        self.open && !self.rows(value, branches).is_empty()
    }

    /// The highlighted row, within the `count` rows there are.
    pub(crate) fn highlight(&self, count: usize) -> usize {
        self.highlight.min(count.saturating_sub(1))
    }

    /// A click in the field opens the list.
    pub(crate) fn open(&mut self) {
        self.open = true;
    }

    /// An edit leaving `value` opens the list on the row of the branch the
    /// trimmed value names exactly, else on its first row.
    pub(crate) fn edited(&mut self, value: &str, branches: &[String]) {
        self.open = true;
        let trimmed = value.trim();
        self.highlight = self
            .rows(value, branches)
            .iter()
            .position(|row| matches!(row, ComboRow::Branch(name) if name == trimmed))
            .unwrap_or(0);
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
    }

    /// Down opens a closed list, else highlights the next row, stopping at
    /// the last.
    pub(crate) fn down(&mut self, value: &str, branches: &[String]) {
        if self.open {
            let count = self.rows(value, branches).len();
            self.highlight = (self.highlight + 1).min(count.saturating_sub(1));
        } else {
            self.open = true;
        }
    }

    /// Up highlights the row before, stopping at the first.
    pub(crate) fn up(&mut self) {
        self.highlight = self.highlight.saturating_sub(1);
    }

    /// The pointer is over row `index`; returns whether that moved the
    /// highlight.
    pub(crate) fn hover(&mut self, index: usize) -> bool {
        let moved = self.highlight != index;
        self.highlight = index;
        moved
    }

    /// Esc closes a list that shows; returns whether it did, so Esc goes no
    /// further.
    pub(crate) fn escape(&mut self, value: &str, branches: &[String]) -> bool {
        let shown = self.shown(value, branches);
        if shown {
            self.open = false;
        }
        shown
    }

    /// Enter commits the highlighted row while the list shows: the text the
    /// field takes, the list closed. `None` leaves Enter to the dialog.
    pub(crate) fn enter(&mut self, value: &str, branches: &[String]) -> Option<String> {
        if !self.shown(value, branches) {
            return None;
        }
        let rows = self.rows(value, branches);
        let index = self.highlight(rows.len());
        self.pick(value, branches, index)
    }

    /// Row `index` was pressed: the text the field takes, the list closed.
    pub(crate) fn pick(
        &mut self,
        value: &str,
        branches: &[String],
        index: usize,
    ) -> Option<String> {
        let row = self.rows(value, branches).into_iter().nth(index)?;
        self.open = false;
        Some(row.value().to_owned())
    }
}

/// Where a list of `wanted` height goes beside a field spanning `field`
/// inside `room` (both vertical): below while it fits there or below has the
/// more space, else above. Returns whether it goes above, and the most
/// height it may take.
pub(crate) fn list_placement(
    room: Range<Pixels>,
    field: Range<Pixels>,
    wanted: Pixels,
) -> (bool, Pixels) {
    let gap = px(EDGE_GAP);
    let below = (room.end - field.end - gap).max(px(0.0));
    let above = (field.start - room.start - gap).max(px(0.0));
    if below >= wanted || below >= above {
        (false, below.min(wanted))
    } else {
        (true, above.min(wanted))
    }
}

#[cfg(test)]
mod tests {
    use super::{ComboRow, Combobox};

    fn branches() -> Vec<String> {
        ["main", "wt/red-fox", "feature/Login"]
            .map(str::to_owned)
            .to_vec()
    }

    fn labels(combo: &Combobox, value: &str) -> Vec<String> {
        combo
            .rows(value, &branches())
            .iter()
            .map(ComboRow::label)
            .collect()
    }

    #[test]
    fn empty_value_offers_every_branch_and_no_create_row() {
        let combo = Combobox::new(true);
        assert_eq!(labels(&combo, ""), ["main", "wt/red-fox", "feature/Login"]);
        assert_eq!(
            labels(&combo, "   "),
            ["main", "wt/red-fox", "feature/Login"]
        );
    }

    #[test]
    fn exact_match_offers_every_branch_and_no_create_row() {
        let combo = Combobox::new(true);
        assert_eq!(
            labels(&combo, "main"),
            ["main", "wt/red-fox", "feature/Login"]
        );
        assert_eq!(
            labels(&combo, " main "),
            ["main", "wt/red-fox", "feature/Login"]
        );
    }

    #[test]
    fn substring_filters_case_insensitively_and_offers_create() {
        let combo = Combobox::new(true);
        assert_eq!(
            labels(&combo, "LOG"),
            ["feature/Login", "Create branch “LOG”"]
        );
    }

    #[test]
    fn no_match_offers_only_create() {
        let combo = Combobox::new(true);
        assert_eq!(labels(&combo, "zzz"), ["Create branch “zzz”"]);
        assert_eq!(
            combo.rows(" zzz ", &branches()),
            [ComboRow::Create("zzz".to_owned())],
            "the typed name trimmed"
        );
    }

    #[test]
    fn base_list_offers_no_create_row_and_hides_when_empty() {
        let mut combo = Combobox::new(false);
        assert_eq!(labels(&combo, "LOG"), ["feature/Login"]);
        assert_eq!(labels(&combo, "zzz"), [] as [std::string::String; 0]);
        combo.edited("zzz", &branches());
        assert!(combo.is_open());
        assert!(!combo.shown("zzz", &branches()), "no empty box");
        assert_eq!(
            combo.enter("zzz", &branches()),
            None,
            "Enter is the dialog's"
        );
        assert!(!combo.escape("zzz", &branches()), "Esc is the dialog's");
    }

    #[test]
    fn edit_opens_on_the_first_row() {
        let mut combo = Combobox::new(true);
        combo.open();
        combo.down("", &branches());
        combo.down("", &branches());
        assert_eq!(combo.highlight(3), 2);
        combo.edited("", &branches());
        assert!(combo.is_open());
        assert_eq!(combo.highlight(3), 0);
    }

    #[test]
    fn exact_match_highlights_its_row() {
        let mut combo = Combobox::new(true);
        combo.edited("wt/red-fox", &branches());
        assert_eq!(combo.highlight(3), 1, "the named branch's row");
        assert_eq!(
            combo.enter("wt/red-fox", &branches()).as_deref(),
            Some("wt/red-fox"),
            "Enter keeps what was typed"
        );
        combo.edited(" feature/Login ", &branches());
        assert_eq!(combo.highlight(3), 2, "trimmed");
        combo.edited("feature/login", &branches());
        assert_eq!(combo.highlight(2), 0, "no exact match: the first row");
    }

    #[test]
    fn down_opens_then_moves_and_stops_at_the_last() {
        let mut combo = Combobox::new(true);
        combo.down("", &branches());
        assert!(combo.is_open(), "the first Down opens");
        assert_eq!(combo.highlight(3), 0, "and moves nothing");
        for _ in 0..5 {
            combo.down("", &branches());
        }
        assert_eq!(combo.highlight(3), 2, "clamped to the last row");
    }

    #[test]
    fn up_stops_at_the_first() {
        let mut combo = Combobox::new(true);
        combo.edited("", &branches());
        combo.down("", &branches());
        combo.up();
        combo.up();
        assert_eq!(combo.highlight(3), 0);
    }

    #[test]
    fn enter_commits_the_highlighted_row_only_while_open() {
        let mut combo = Combobox::new(true);
        assert_eq!(combo.enter("", &branches()), None, "closed: Enter submits");
        combo.down("", &branches());
        combo.down("", &branches());
        assert_eq!(combo.enter("", &branches()).as_deref(), Some("wt/red-fox"));
        assert!(!combo.is_open(), "a commit closes");
    }

    #[test]
    fn enter_on_the_create_row_commits_the_typed_name() {
        let mut combo = Combobox::new(true);
        combo.edited("zzz ", &branches());
        combo.down("zzz ", &branches());
        assert_eq!(combo.enter("zzz ", &branches()).as_deref(), Some("zzz"));
    }

    #[test]
    fn escape_closes_an_open_list_only() {
        let mut combo = Combobox::new(true);
        assert!(
            !combo.escape("", &branches()),
            "closed: Esc is the dialog's"
        );
        combo.open();
        assert!(combo.escape("", &branches()));
        assert!(!combo.is_open());
    }

    #[test]
    fn hover_moves_the_highlight_and_pick_commits_that_row() {
        let mut combo = Combobox::new(true);
        combo.open();
        assert!(combo.hover(2));
        assert!(!combo.hover(2), "no move, no redraw");
        assert_eq!(
            combo.enter("", &branches()).as_deref(),
            Some("feature/Login")
        );
        assert_eq!(combo.pick("log", &branches(), 1).as_deref(), Some("log"));
        assert_eq!(combo.pick("log", &branches(), 5), None);
    }
}
