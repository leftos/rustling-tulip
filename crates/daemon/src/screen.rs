//! A rendered terminal grid for one PTY session.
//!
//! Fed from the PTY output by the session's status watcher, resized
//! with the PTY, and seeded by the tracer's replay after a reattach, so a
//! reader sees the frame the user sees rather than a byte tail of superseded
//! repaints. Reads come from region queries over the visible rows and from
//! the last OSC 0/2 title the stream set.

use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Processor;

use crate::osc_title::PromptMarks;

pub mod dialogs;
#[cfg(test)]
pub(crate) mod fixture_screen;
pub mod rules;

/// The character a TUI draws a horizontal rule with.
const RULE_CHAR: char = '─';

/// The grid width a reattached session starts at: the daemon does not know
/// the size the tracer's PTY last had, so the replay is applied to a grid of
/// this size and the PTY is moved to it as well; the client's first `Resize`
/// moves both together.
pub const REATTACH_COLS: u16 = 120;

/// The grid height a reattached session starts at.
pub const REATTACH_ROWS: u16 = 32;

/// The grid of one session, shared between the watcher that feeds it and its
/// readers.
pub type SharedScreen = Arc<Mutex<Screen>>;

/// The dimensions of a grid, as [`Dimensions`] wants them.
#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Keeps the last window title the stream sets.
struct TitleListener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => *crate::sync::lock(&self.title) = Some(title),
            Event::ResetTitle => *crate::sync::lock(&self.title) = None,
            _ => {}
        }
    }
}

/// The rendered grid of one PTY session, without scrollback history.
pub struct Screen {
    term: Term<TitleListener>,
    parser: Processor,
    cols: usize,
    rows: usize,
    title: Arc<Mutex<Option<String>>>,
    /// How many bytes of output [`Self::feed`] has applied.
    fed: u64,
    /// The shell's OSC 133 marks, which the grid itself drops.
    marks: PromptMarks,
}

impl Screen {
    /// A blank grid `cols` by `rows`; a zero dimension becomes one.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
        let size = Size { cols, rows };
        let title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let listener = TitleListener {
            title: Arc::clone(&title),
        };
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, listener),
            parser: Processor::new(),
            cols,
            rows,
            title,
            fed: 0,
            marks: PromptMarks::default(),
        }
    }

    /// Applies PTY output to the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.marks.feed(bytes);
        let count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.fed = self.fed.wrapping_add(count);
    }

    /// How many bytes of output the grid has been fed, so a reader can tell
    /// whether output arrived since it last looked.
    #[must_use]
    pub fn fed(&self) -> u64 {
        self.fed
    }

    /// Drops any escape sequence the parser had half read, so the rest of the
    /// stream starts from the ground state instead of being swallowed, and
    /// forgets the shell's last OSC 133 mark, since the lost bytes may have
    /// held a command's start.
    pub fn reset_parser(&mut self) {
        self.parser = Processor::new();
        self.marks.forget();
    }

    /// Whether the shell, which marks its commands with OSC 133, last marked
    /// a prompt start after any command's start or end.
    #[must_use]
    pub fn at_marked_prompt(&self) -> bool {
        self.marks.at_prompt()
    }

    /// Resizes the grid with the PTY. A zero dimension is ignored: with no
    /// history a collapsed grid would lose the frame the PTY still shows.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            tracing::debug!(cols, rows, "screen: ignoring a zero-size resize");
            return;
        }
        let size = Size {
            cols: usize::from(cols),
            rows: usize::from(rows),
        };
        self.term.resize(size);
        self.cols = size.cols;
        self.rows = size.rows;
    }

    /// The grid's size as `(cols, rows)`, as last set.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let narrow = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        (narrow(self.cols), narrow(self.rows))
    }

    /// Every visible row, top to bottom, with trailing spaces trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.visible_rows(false)
    }

    /// Every visible row as [`Screen::rows`] gives it, with every character
    /// not drawn dim (SGR 2) replaced by a space, trailing spaces trimmed:
    /// `dim_rows()[r]` lines up char for char with `rows()[r]`.
    #[must_use]
    pub fn dim_rows(&self) -> Vec<String> {
        self.visible_rows(true)
    }

    /// Every visible row, top to bottom, trailing spaces trimmed; with
    /// `dim_only`, a space for each character not drawn dim.
    fn visible_rows(&self, dim_only: bool) -> Vec<String> {
        let grid = self.term.grid();
        (0..self.rows)
            .map(|index| {
                let row = &grid[Line::from(index)];
                let mut text = String::with_capacity(self.cols);
                for column in 0..self.cols {
                    let cell = &row[Column(column)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let shown = !dim_only || cell.flags.contains(Flags::DIM);
                    text.push(if shown { cell.c } else { ' ' });
                    if let Some(extra) = cell.zerowidth() {
                        text.extend(extra.iter().map(|&c| if shown { c } else { ' ' }));
                    }
                }
                text.trim_end().to_owned()
            })
            .collect()
    }

    /// The last `n` rows whose trimmed text is non-empty, top to bottom.
    #[must_use]
    pub fn bottom_non_empty_lines(&self, n: usize) -> Vec<String> {
        last_non_empty(&self.rows(), n)
    }

    /// The bottom logical line: the last non-empty row, soft-wrapped rows joined.
    #[must_use]
    pub fn bottom_logical_line(&self) -> Option<String> {
        let grid = self.term.grid();
        let row_text = |index: usize| {
            let row = &grid[Line::from(index)];
            let mut text = String::with_capacity(self.cols);
            for column in 0..self.cols {
                let cell = &row[Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                text.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
            }
            text
        };
        let wraps = |index: usize| {
            grid[Line::from(index)][Column(self.cols - 1)]
                .flags
                .contains(Flags::WRAPLINE)
        };
        let last = (0..self.rows)
            .rev()
            .find(|&index| !row_text(index).trim_end().is_empty())?;
        let mut first = last;
        while first > 0 && wraps(first - 1) {
            first -= 1;
        }
        let joined: String = (first..=last).map(row_text).collect();
        Some(joined.trim_end().to_owned())
    }

    /// The rows below the last rule row, trailing empty rows dropped.
    #[must_use]
    pub fn after_last_horizontal_rule(&self) -> Vec<String> {
        let rows = self.rows();
        let rules = rule_indices(&rows, self.cols);
        let Some(&last) = rules.last() else {
            return Vec::new();
        };
        let mut below = rows[last + 1..].to_vec();
        while matches!(below.last(), Some(row) if row.is_empty()) {
            below.pop();
        }
        below
    }

    /// The rows strictly between the last two rule rows.
    #[must_use]
    pub fn prompt_box_body(&self) -> Vec<String> {
        let rows = self.rows();
        let rules = rule_indices(&rows, self.cols);
        if rules.len() < 2 {
            return Vec::new();
        }
        rows[rules[rules.len() - 2] + 1..rules[rules.len() - 1]].to_vec()
    }

    /// The last `n` non-empty rows above the second-to-last rule row.
    #[must_use]
    pub fn above_prompt_box(&self, n: usize) -> Vec<String> {
        let rows = self.rows();
        let rules = rule_indices(&rows, self.cols);
        if rules.len() < 2 {
            return Vec::new();
        }
        last_non_empty(&rows[..rules[rules.len() - 2]], n)
    }

    /// The last OSC 0/2 title the stream set.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        crate::sync::lock(&self.title).clone()
    }

    /// The title a session had before this screen existed, kept until the
    /// stream sets a title or replaces one: a seed only fills a screen whose
    /// stream has set no title yet, and `None` seeds nothing.
    pub fn seed_title(&self, title: Option<&str>) {
        let Some(title) = title else {
            return;
        };
        let mut current = crate::sync::lock(&self.title);
        current.get_or_insert_with(|| title.to_owned());
    }
}

/// Whether `row` is a horizontal rule: a row that starts and ends with the
/// rule character, a label allowed in between, holding at least half the
/// width in that character.
fn is_rule(row: &str, cols: usize) -> bool {
    let trimmed = row.trim();
    let rules = row.chars().filter(|&c| c == RULE_CHAR).count();
    trimmed.starts_with(RULE_CHAR) && trimmed.ends_with(RULE_CHAR) && rules >= cols / 2
}

/// The indexes of the rule rows in `rows`.
fn rule_indices(rows: &[String], cols: usize) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| is_rule(row, cols))
        .map(|(index, _)| index)
        .collect()
}

/// The last `n` non-empty rows of `rows`, in their order.
fn last_non_empty(rows: &[String], n: usize) -> Vec<String> {
    let mut lines: Vec<String> = rows.iter().filter(|row| !row.is_empty()).cloned().collect();
    let excess = lines.len().saturating_sub(n);
    lines.drain(..excess);
    lines
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn dim_rows_keep_only_the_dim_text_at_its_columns() {
        let screen =
            fixture_screen::screen_from(include_str!("screen/fixtures/claude--suggestion.txt"));
        let rows = screen.rows();
        let dim = screen.dim_rows();
        assert_eq!(dim.len(), rows.len());
        let index = rows
            .iter()
            .position(|row| row.starts_with('❯') && row.ends_with("write todo.py"))
            .expect("the capture's box shows the suggestion");
        assert_eq!(dim[index], "  write todo.py", "the prompt is not dim");
        let column = |row: &str| row.chars().position(|c| c == 'w');
        assert_eq!(column(&dim[index]), column(&rows[index]));
        let rule = rows
            .iter()
            .position(|row| row.starts_with('─'))
            .expect("the capture has a rule");
        assert_eq!(dim[rule], "", "a coloured rule is not dim");
    }

    #[test]
    fn feed_counts_the_bytes_it_applies() {
        let mut screen = Screen::new(80, 10);
        assert_eq!(screen.fed(), 0);
        screen.feed(b"hello");
        screen.feed(b"\x1b[2J");
        screen.feed(b"");
        assert_eq!(screen.fed(), 9, "every byte counts, escape sequences too");
        screen.reset_parser();
        screen.resize(40, 5);
        assert_eq!(screen.fed(), 9, "only feeding moves the count");
    }

    #[test]
    fn a_spinner_repainted_in_place_leaves_one_spinner_row() {
        let mut screen = Screen::new(80, 10);
        screen.feed(b"line one\r\nline two\r\n");
        for glyph in ["·", "✢", "*", "✶", "✻", "✽"] {
            screen
                .feed(format!("\x1b[3;1H\x1b[2K{glyph} Cooking… (21s · ↓ 1.1k tokens)").as_bytes());
        }
        assert_eq!(
            screen.bottom_non_empty_lines(12),
            ["line one", "line two", "✽ Cooking… (21s · ↓ 1.1k tokens)"]
        );
    }

    #[test]
    fn regions_of_an_idle_claude_box() {
        let mut screen = Screen::new(40, 12);
        let rule = "─".repeat(40);
        let frame = [
            "✻ Baked for 22s · done 5:13 PM",
            "",
            "                    ● high · /effort",
            rule.as_str(),
            "❯ ",
            rule.as_str(),
            "  ? for shortcuts",
        ];
        screen.feed(frame.join("\r\n").as_bytes());
        assert_eq!(screen.prompt_box_body(), ["❯"]);
        assert_eq!(screen.after_last_horizontal_rule(), ["  ? for shortcuts"]);
        assert_eq!(
            screen.above_prompt_box(2),
            [
                "✻ Baked for 22s · done 5:13 PM",
                "                    ● high · /effort"
            ]
        );
        assert_eq!(screen.bottom_non_empty_lines(1), ["  ? for shortcuts"]);
    }

    #[test]
    fn a_labelled_top_rule_still_bounds_the_box() {
        let mut screen = Screen::new(40, 10);
        let frame = [
            "· Cooking… (21s · ↓ 1.1k tokens)".to_owned(),
            format!("{} my-branch ─", "─".repeat(28)),
            "❯ ".to_owned(),
            "─".repeat(40),
            "  ⏵⏵ auto mode on · 1 shell".to_owned(),
        ];
        screen.feed(frame.join("\r\n").as_bytes());
        assert_eq!(screen.prompt_box_body(), ["❯"]);
        assert_eq!(
            screen.above_prompt_box(1),
            ["· Cooking… (21s · ↓ 1.1k tokens)"]
        );
        assert_eq!(
            screen.after_last_horizontal_rule(),
            ["  ⏵⏵ auto mode on · 1 shell"]
        );
    }

    #[test]
    fn no_rule_means_no_box() {
        let mut screen = Screen::new(40, 6);
        screen.feed(b"PS C:\\> ");
        assert_eq!(screen.prompt_box_body(), Vec::<String>::new());
        assert_eq!(screen.after_last_horizontal_rule(), Vec::<String>::new());
        assert_eq!(screen.above_prompt_box(3), Vec::<String>::new());
        assert_eq!(screen.bottom_non_empty_lines(3), ["PS C:\\>"]);
    }

    #[test]
    fn rule_rows_follow_the_worked_examples() {
        let cols = 20;
        assert!(is_rule("────────────────────", cols));
        assert!(is_rule("──────────", cols));
        assert!(!is_rule("─────────", cols));
        assert!(is_rule("───── x ─────", cols));
        assert!(is_rule("  ────────────  ", cols));
        assert!(!is_rule("", cols));
        assert!(!is_rule("❯ ", cols));
        assert!(!is_rule("x ──────────", cols));
    }

    #[test]
    fn osc_title_is_kept_and_reset() {
        let mut screen = Screen::new(40, 6);
        screen.feed("\x1b]0;◐ Fixing tests\x07".as_bytes());
        assert_eq!(screen.title(), Some("◐ Fixing tests".to_owned()));
        screen.feed("\x1b]2;✳ Claude Code\x07".as_bytes());
        assert_eq!(screen.title(), Some("✳ Claude Code".to_owned()));
        screen.feed("\x1b]0;\x07".as_bytes());
        // `vte` hands the trimmed trailing parameter to alacritty, which stores
        // whatever it is given (`term/mod.rs` `set_title`: `self.title.clone_from(&title);`)
        // and sends `Event::ResetTitle` only for `None`. An empty title is
        // therefore stored as the empty string, not as a reset.
        assert_eq!(screen.title(), Some(String::new()));
    }

    #[test]
    fn a_seeded_title_reads_until_the_stream_sets_one() {
        let mut screen = Screen::new(40, 6);
        screen.seed_title(Some("✳ Fix login"));
        assert_eq!(screen.title(), Some("✳ Fix login".to_owned()));
        screen.feed("\x1b]0;✳ Other\x07".as_bytes());
        assert_eq!(screen.title(), Some("✳ Other".to_owned()));
    }

    #[test]
    fn a_seed_never_replaces_a_title_the_stream_set() {
        let mut screen = Screen::new(40, 6);
        screen.feed("\x1b]0;◐ Fixing tests\x07".as_bytes());
        screen.seed_title(Some("✳ Fix login"));
        assert_eq!(screen.title(), Some("◐ Fixing tests".to_owned()));
    }

    #[test]
    fn a_seed_of_none_leaves_no_title() {
        let screen = Screen::new(40, 6);
        screen.seed_title(None);
        assert_eq!(screen.title(), None);
    }

    #[test]
    fn a_title_reset_clears_a_seed() {
        let mut screen = Screen::new(40, 6);
        screen.seed_title(Some("✳ Fix login"));
        // An empty OSC 0 reaches alacritty as the empty title rather than a
        // reset (see `osc_title_is_kept_and_reset`), so the stream's empty
        // title is what replaces the seed.
        screen.feed("\x1b]0;\x07".as_bytes());
        assert_eq!(screen.title(), Some(String::new()));
    }

    #[test]
    fn resize_keeps_the_bottom_rows() {
        let mut screen = Screen::new(40, 6);
        screen.feed(b"r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6");
        screen.resize(40, 3);
        assert_eq!(screen.bottom_non_empty_lines(3), ["r4", "r5", "r6"]);
    }

    #[test]
    fn a_wide_char_is_one_char() {
        let mut screen = Screen::new(20, 4);
        screen.feed("日本 ok".as_bytes());
        assert_eq!(screen.rows()[0], "日本 ok");
    }

    #[test]
    fn a_combining_mark_stays_with_its_letter() {
        let mut screen = Screen::new(20, 4);
        // Built rather than written out so the source stays NFC.
        let combining = char::from_u32(0x0301).expect("a combining acute accent");
        let typed = format!("cafe{combining}");
        screen.feed(typed.as_bytes());
        assert_eq!(screen.rows()[0], typed);
    }

    #[test]
    fn a_zero_size_resize_keeps_the_grid() {
        let mut screen = Screen::new(40, 6);
        screen.feed(b"keep me");
        screen.resize(0, 6);
        screen.resize(40, 0);
        assert_eq!(screen.rows()[0], "keep me");
        assert_eq!(screen.rows().len(), 6);
    }

    #[test]
    fn a_reset_parser_drops_a_cut_sequence() {
        let mut screen = Screen::new(40, 6);
        screen.feed(b"\x1b]0;half a title");
        screen.reset_parser();
        screen.feed(b"\r\nafter");
        assert_eq!(screen.bottom_non_empty_lines(1), ["after"]);
    }
}
