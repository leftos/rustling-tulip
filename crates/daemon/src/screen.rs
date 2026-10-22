//! A rendered terminal grid for one PTY session.
//!
//! Fed from the same PTY output broadcast the status watchers read, resized
//! with the PTY, and seeded by the tracer's replay after a reattach, so a
//! reader sees the frame the user sees rather than a byte tail of superseded
//! repaints. Reads come from region queries over the visible rows and from
//! the last OSC 0/2 title the stream set.

use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::Processor;
use tokio::sync::broadcast;

/// The character a TUI draws a horizontal rule with.
const RULE_CHAR: char = '─';

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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read through the screen's readers, not the field")
)]
pub struct Screen {
    term: Term<TitleListener>,
    parser: Processor,
    cols: usize,
    rows: usize,
    title: Arc<Mutex<Option<String>>>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the region readers are the screen rules' API; only the tests call them yet"
    )
)]
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
        }
    }

    /// Applies PTY output to the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resizes the grid with the PTY; a zero dimension becomes one.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        self.term.resize(size);
        self.cols = size.cols;
        self.rows = size.rows;
    }

    /// Every visible row, top to bottom, with trailing spaces trimmed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        let grid = self.term.grid();
        let first = grid.topmost_line();
        (0..self.rows)
            .map(|index| {
                let row = &grid[first + index];
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

/// Feeds `screen` from `output` until the broadcast closes.
pub fn watch(screen: SharedScreen, session_id: String, mut output: broadcast::Receiver<Vec<u8>>) {
    tokio::spawn(async move {
        loop {
            match output.recv().await {
                Ok(bytes) => crate::sync::lock(&screen).feed(&bytes),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        session_id = %session_id,
                        skipped,
                        "screen: output lagged; grid may be stale until the next repaint"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::time::Duration;

    use super::*;

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

    #[tokio::test]
    async fn watch_feeds_the_grid_and_ends_on_close() {
        let shared: SharedScreen = Arc::new(Mutex::new(Screen::new(20, 4)));
        let (tx, rx) = broadcast::channel(16);
        watch(Arc::clone(&shared), "s".to_owned(), rx);
        tx.send(b"hello".to_vec()).expect("send the output");
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if crate::sync::lock(&shared).rows()[0] == "hello" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the watcher feeds the grid");
    }
}
